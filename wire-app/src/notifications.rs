use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use egui::{
    epaint::Shadow, Align, Color32, CornerRadius, Frame, Id, Layout, Margin, RichText, Stroke,
    Vec2, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId, WindowLevel,
};
use tracing::info;

use crate::theme::{sans, ui_font_size, visuals_for, Palette, Theme};

const HOST_WIDTH: f32 = 348.0;
const INITIAL_HOST_HEIGHT: f32 = 112.0;
const CARD_WIDTH: f32 = 332.0;
const OUTER_PADDING: f32 = 8.0;
const STACK_GAP: f32 = 7.0;
const RIGHT_MARGIN: f32 = 20.0;
const TOP_MARGIN: f32 = 20.0;
const ENTER_TIME: Duration = Duration::from_millis(210);
const LEAVE_TIME: Duration = Duration::from_millis(180);
const GROUP_WINDOW: Duration = Duration::from_secs(30);
const MAX_VISIBLE: usize = 4;
const MAX_FUSED_ROWS: usize = 4;

fn notification_viewport_id() -> ViewportId {
    ViewportId::from_hash_of("wire-notification-overlay")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NotificationAction {
    OpenConversation(String),
    OpenCalls,
    AcceptCall(String),
    DeclineCall(String),
}

#[derive(Clone)]
struct ActionButton {
    label: String,
    action: NotificationAction,
    emphasized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationKind {
    Message,
    IncomingCall,
    Success,
    Error,
    Info,
}

impl NotificationKind {
    fn default_lifetime(self) -> Option<Duration> {
        match self {
            Self::Message => Some(Duration::from_secs(7)),
            Self::IncomingCall => None,
            Self::Success => Some(Duration::from_secs(5)),
            Self::Error => Some(Duration::from_secs(9)),
            Self::Info => Some(Duration::from_secs(6)),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Message => "MESSAGE",
            Self::IncomingCall => "INCOMING CALL",
            Self::Success => "WIRE",
            Self::Error => "ATTENTION",
            Self::Info => "WIRE",
        }
    }

    fn can_fuse(self) -> bool {
        matches!(self, Self::Message)
    }
}

#[derive(Clone)]
struct NotificationEntry {
    title: String,
    body: String,
    action: Option<NotificationAction>,
}

struct NotificationGroup {
    id: u64,
    key: Option<String>,
    kind: NotificationKind,
    entries: VecDeque<NotificationEntry>,
    hidden_entries: usize,
    buttons: Vec<ActionButton>,
    created_at: Instant,
    expires_at: Option<Instant>,
    leaving_at: Option<Instant>,
    render_y: f32,
    measured_height: f32,
}

impl NotificationGroup {
    fn height(&self) -> f32 {
        self.measured_height
    }

    fn estimated_height(&self) -> f32 {
        if self.is_fused() {
            30.0 + self.entries.len() as f32 * 45.0
                + if self.hidden_entries > 0 { 22.0 } else { 0.0 }
        } else {
            match self.kind {
                NotificationKind::IncomingCall => 118.0,
                _ if self.buttons.is_empty() => 78.0,
                _ => 108.0,
            }
        }
    }

    fn is_fused(&self) -> bool {
        self.kind.can_fuse() && (self.entries.len() > 1 || self.hidden_entries > 0)
    }

    fn opacity(&self, now: Instant) -> f32 {
        let entering = (now.saturating_duration_since(self.created_at).as_secs_f32()
            / ENTER_TIME.as_secs_f32())
        .clamp(0.0, 1.0);
        let leaving = self
            .leaving_at
            .map(|started| {
                1.0 - (now.saturating_duration_since(started).as_secs_f32()
                    / LEAVE_TIME.as_secs_f32())
                .clamp(0.0, 1.0)
            })
            .unwrap_or(1.0);
        smoothstep(entering) * smoothstep(leaving)
    }

    fn is_animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created_at) < ENTER_TIME
            || self
                .leaving_at
                .is_some_and(|started| now.saturating_duration_since(started) < LEAVE_TIME)
    }
}

fn smoothstep(value: f32) -> f32 {
    value * value * (3.0 - 2.0 * value)
}

struct NotificationStore {
    visible: VecDeque<NotificationGroup>,
    pending: VecDeque<NotificationGroup>,
    next_id: u64,
    last_frame: Instant,
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self {
            visible: VecDeque::new(),
            pending: VecDeque::new(),
            next_id: 1,
            last_frame: Instant::now(),
        }
    }
}

struct NotificationSpec {
    group_key: Option<String>,
    kind: NotificationKind,
    title: String,
    body: String,
    action: Option<NotificationAction>,
    buttons: Vec<ActionButton>,
}

enum NotificationCommand {
    Push(NotificationSpec),
    DismissKey(String),
}

impl NotificationStore {
    fn push(&mut self, spec: NotificationSpec, now: Instant) {
        if spec.kind.can_fuse() {
            if let Some(key) = spec.group_key.as_deref() {
                if let Some(group) = self.visible.iter_mut().find(|group| {
                    group.key.as_deref() == Some(key)
                        && group.kind == spec.kind
                        && now.saturating_duration_since(group.created_at) <= GROUP_WINDOW
                        && group.leaving_at.is_none()
                }) {
                    group.entries.push_front(NotificationEntry {
                        title: spec.title,
                        body: spec.body,
                        action: spec.action,
                    });
                    if group.entries.len() > MAX_FUSED_ROWS {
                        group.entries.pop_back();
                        group.hidden_entries += 1;
                    }
                    group.created_at = now;
                    group.expires_at = spec.kind.default_lifetime().map(|life| now + life);
                    group.measured_height = group.estimated_height();
                    return;
                }
            }
        }

        let estimated_height = match spec.kind {
            NotificationKind::IncomingCall => 118.0,
            _ if spec.buttons.is_empty() => 78.0,
            _ => 108.0,
        };
        let group = NotificationGroup {
            id: self.next_id,
            key: spec.group_key,
            kind: spec.kind,
            entries: VecDeque::from([NotificationEntry {
                title: spec.title,
                body: spec.body,
                action: spec.action,
            }]),
            hidden_entries: 0,
            buttons: spec.buttons,
            created_at: now,
            expires_at: spec.kind.default_lifetime().map(|life| now + life),
            leaving_at: None,
            render_y: -20.0,
            measured_height: estimated_height,
        };
        self.next_id += 1;

        if self.visible.len() < MAX_VISIBLE {
            self.visible.push_front(group);
        } else {
            self.pending.push_back(group);
        }
    }

    fn dismiss_key(&mut self, key: &str, now: Instant) {
        for group in &mut self.visible {
            if group.key.as_deref() == Some(key) && group.leaving_at.is_none() {
                group.leaving_at = Some(now);
            }
        }
        self.pending
            .retain(|group| group.key.as_deref() != Some(key));
    }

    fn dismiss_id(&mut self, id: u64, now: Instant) {
        if let Some(group) = self.visible.iter_mut().find(|group| group.id == id) {
            group.leaving_at.get_or_insert(now);
        }
    }

    fn clear(&mut self) {
        self.visible.clear();
        self.pending.clear();
    }

    fn advance(&mut self, now: Instant) {
        for group in &mut self.visible {
            if group.leaving_at.is_none() && group.expires_at.is_some_and(|expiry| now >= expiry) {
                group.leaving_at = Some(now);
            }
        }
        self.visible.retain(|group| {
            !group
                .leaving_at
                .is_some_and(|started| now.saturating_duration_since(started) >= LEAVE_TIME)
        });

        while self.visible.len() < MAX_VISIBLE {
            let Some(mut group) = self.pending.pop_front() else {
                break;
            };
            group.created_at = now;
            group.expires_at = group.kind.default_lifetime().map(|life| now + life);
            group.render_y = -20.0;
            self.visible.push_front(group);
        }
    }

    fn layout(&mut self, now: Instant) -> f32 {
        let dt = now
            .saturating_duration_since(self.last_frame)
            .as_secs_f32()
            .min(0.1);
        self.last_frame = now;
        let follow = 1.0 - (-18.0 * dt).exp();
        let mut target_y = OUTER_PADDING;
        for group in &mut self.visible {
            group.render_y += (target_y - group.render_y) * follow;
            target_y += group.height() + STACK_GAP;
        }
        (target_y - STACK_GAP + OUTER_PADDING).max(1.0)
    }

    fn has_notifications(&self) -> bool {
        !self.visible.is_empty() || !self.pending.is_empty()
    }

    fn next_repaint(&self, now: Instant) -> Option<Duration> {
        if self.visible.iter().any(|group| {
            group.is_animating(now)
                || (group.render_y
                    - self
                        .visible
                        .iter()
                        .take_while(|candidate| candidate.id != group.id)
                        .map(|candidate| candidate.height() + STACK_GAP)
                        .sum::<f32>()
                    - OUTER_PADDING)
                    .abs()
                    > 0.5
        }) {
            return Some(Duration::from_millis(16));
        }
        self.visible
            .iter()
            .filter_map(|group| group.expires_at)
            .min()
            .map(|expiry| expiry.saturating_duration_since(now))
    }
}

struct NotificationRuntime {
    store: NotificationStore,
    command_rx: mpsc::Receiver<NotificationCommand>,
    resize_target: Option<Vec2>,
    settled_frames: u8,
}

impl NotificationRuntime {
    fn apply_commands(&mut self, now: Instant) {
        while let Ok(command) = self.command_rx.try_recv() {
            match command {
                NotificationCommand::Push(spec) => self.store.push(spec, now),
                NotificationCommand::DismissKey(key) => self.store.dismiss_key(&key, now),
            }
        }
    }
}

pub(crate) struct NotificationService {
    runtime: Arc<Mutex<NotificationRuntime>>,
    active: Arc<AtomicBool>,
    viewport_ready: Arc<AtomicBool>,
    command_tx: mpsc::Sender<NotificationCommand>,
    action_tx: mpsc::Sender<NotificationAction>,
    action_rx: mpsc::Receiver<NotificationAction>,
}

impl Default for NotificationService {
    fn default() -> Self {
        let (action_tx, action_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        Self {
            runtime: Arc::new(Mutex::new(NotificationRuntime {
                store: NotificationStore::default(),
                command_rx,
                resize_target: None,
                settled_frames: 0,
            })),
            active: Arc::new(AtomicBool::new(false)),
            viewport_ready: Arc::new(AtomicBool::new(false)),
            command_tx,
            action_tx,
            action_rx,
        }
    }
}

impl NotificationService {
    pub(crate) fn message(
        &self,
        conversation_id: String,
        conversation_title: String,
        author: String,
        body: String,
    ) {
        self.push(NotificationSpec {
            group_key: Some(format!("conversation:{conversation_id}")),
            kind: NotificationKind::Message,
            title: if conversation_title == author {
                author
            } else {
                format!("{author} - {conversation_title}")
            },
            body,
            action: Some(NotificationAction::OpenConversation(conversation_id)),
            buttons: Vec::new(),
        });
    }

    pub(crate) fn incoming_call(&self, node_id: String, peer_name: String) {
        self.push(NotificationSpec {
            group_key: Some(format!("call:{node_id}")),
            kind: NotificationKind::IncomingCall,
            title: peer_name,
            body: "Wants to start a voice call".to_owned(),
            action: Some(NotificationAction::OpenCalls),
            buttons: vec![
                ActionButton {
                    label: "Decline".to_owned(),
                    action: NotificationAction::DeclineCall(node_id.clone()),
                    emphasized: false,
                },
                ActionButton {
                    label: "Accept".to_owned(),
                    action: NotificationAction::AcceptCall(node_id),
                    emphasized: true,
                },
            ],
        });
    }

    pub(crate) fn error(&self, key: impl Into<String>, title: impl Into<String>, body: String) {
        self.push(NotificationSpec {
            group_key: Some(key.into()),
            kind: NotificationKind::Error,
            title: title.into(),
            body,
            action: None,
            buttons: Vec::new(),
        });
    }

    pub(crate) fn success(
        &self,
        key: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.push(NotificationSpec {
            group_key: Some(key.into()),
            kind: NotificationKind::Success,
            title: title.into(),
            body: body.into(),
            action: None,
            buttons: Vec::new(),
        });
    }

    pub(crate) fn info(
        &self,
        key: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.push(NotificationSpec {
            group_key: Some(key.into()),
            kind: NotificationKind::Info,
            title: title.into(),
            body: body.into(),
            action: None,
            buttons: Vec::new(),
        });
    }

    fn push(&self, spec: NotificationSpec) {
        if self
            .command_tx
            .send(NotificationCommand::Push(spec))
            .is_ok()
        {
            self.active.store(true, Ordering::Release);
        }
    }

    pub(crate) fn dismiss_key(&self, key: &str) {
        let _ = self
            .command_tx
            .send(NotificationCommand::DismissKey(key.to_owned()));
    }

    pub(crate) fn try_action(&self) -> Option<NotificationAction> {
        self.action_rx.try_recv().ok()
    }

    pub(crate) fn show(&self, ctx: &egui::Context, theme: Theme) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }

        let monitor_size = ctx.input(|input| input.viewport().monitor_size);
        let initial_position = monitor_size.map(|size| {
            egui::pos2(
                (size.x - HOST_WIDTH - RIGHT_MARGIN).max(0.0),
                platform_top_margin(),
            )
        });
        let mut builder = ViewportBuilder::default()
            .with_title("Wire Notifications")
            .with_inner_size([HOST_WIDTH, INITIAL_HOST_HEIGHT])
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_active(false)
            .with_visible(self.viewport_ready.load(Ordering::Acquire))
            .with_taskbar(false)
            .with_window_level(WindowLevel::AlwaysOnTop);
        if let Some(position) = initial_position {
            builder = builder.with_position(position);
        }

        let runtime = Arc::clone(&self.runtime);
        let active = Arc::clone(&self.active);
        let viewport_ready = Arc::clone(&self.viewport_ready);
        let action_tx = self.action_tx.clone();
        ctx.request_repaint_of(notification_viewport_id());
        ctx.show_viewport_deferred(
            notification_viewport_id(),
            builder,
            move |overlay_ctx, class| {
                render_overlay(
                    overlay_ctx,
                    class,
                    &runtime,
                    &active,
                    &viewport_ready,
                    &action_tx,
                    theme,
                );
            },
        );
    }
}

fn platform_top_margin() -> f32 {
    #[cfg(target_os = "macos")]
    {
        36.0
    }
    #[cfg(not(target_os = "macos"))]
    {
        TOP_MARGIN
    }
}

fn render_overlay(
    ctx: &egui::Context,
    class: ViewportClass,
    runtime: &Arc<Mutex<NotificationRuntime>>,
    active: &Arc<AtomicBool>,
    viewport_ready: &Arc<AtomicBool>,
    action_tx: &mpsc::Sender<NotificationAction>,
    theme: Theme,
) {
    let now = Instant::now();
    let pal = Palette::for_theme(theme);
    ctx.set_visuals(visuals_for(&pal));

    let close_requested = ctx.input(|input| input.viewport().close_requested());
    let Ok(mut runtime) = runtime.lock() else {
        return;
    };
    runtime.apply_commands(now);
    let NotificationRuntime {
        store,
        resize_target,
        settled_frames,
        ..
    } = &mut *runtime;
    if close_requested {
        info!("notification viewport close requested");
        store.clear();
        active.store(false, Ordering::Release);
        viewport_ready.store(false, Ordering::Release);
        ctx.request_repaint_of(ViewportId::ROOT);
        return;
    }
    store.advance(now);
    if !store.has_notifications() {
        active.store(false, Ordering::Release);
        viewport_ready.store(false, Ordering::Release);
        ctx.request_repaint_of(ViewportId::ROOT);
        return;
    }

    let host_height = store.layout(now);
    if class != ViewportClass::Embedded {
        let current_size = ctx.input(|input| input.viewport().inner_rect.map(|rect| rect.size()));
        let desired_size = Vec2::new(HOST_WIDTH, host_height);
        let target_changed = resize_target
            .is_none_or(|target| (target - desired_size).length() > 1.0);
        if target_changed {
            *resize_target = Some(desired_size);
            *settled_frames = 0;
        }
        if current_size.is_none_or(|size| (size - desired_size).length() > 1.0) {
            // On Windows, resizing a visible transparent WGPU surface can leave
            // pixels from the initial opaque backing store in the newly exposed
            // region. Keep the viewport hidden until it has rendered a complete
            // frame at its final size, then reveal it atomically.
            viewport_ready.store(false, Ordering::Release);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(desired_size));
            *settled_frames = 0;
            ctx.request_repaint();
            ctx.request_repaint_of(ViewportId::ROOT);
        } else if *settled_frames == 0 {
            *settled_frames = 1;
            ctx.request_repaint();
        } else if !viewport_ready.swap(true, Ordering::AcqRel) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    egui::CentralPanel::default()
        .frame(Frame::NONE)
        .show(ctx, |_ui| {});

    let mut dismiss = Vec::new();
    for group in &mut store.visible {
        let opacity = group.opacity(now);
        let x_offset = (1.0 - opacity) * 28.0;
        let position = egui::pos2(OUTER_PADDING + x_offset, group.render_y);
        let area = egui::Area::new(Id::new(("notification-group", group.id)))
            .order(egui::Order::Foreground)
            .fixed_pos(position)
            .show(ctx, |ui| render_group(ui, group, &pal, opacity, action_tx));
        let measured_height = area.response.rect.height();
        if measured_height > 1.0 && (group.measured_height - measured_height).abs() > 0.5 {
            group.measured_height = measured_height;
            ctx.request_repaint();
        }
        if area.response.hovered() {
            if group.expires_at.is_some() && group.leaving_at.is_none() {
                group.expires_at = group.kind.default_lifetime().map(|life| now + life);
            }
        }
        if area.inner {
            dismiss.push(group.id);
        }
    }
    for id in dismiss {
        store.dismiss_id(id, now);
    }

    if let Some(delay) = store.next_repaint(now) {
        ctx.request_repaint_after(delay.max(Duration::from_millis(1)));
    }
}

fn render_group(
    ui: &mut egui::Ui,
    group: &NotificationGroup,
    pal: &Palette,
    opacity: f32,
    action_tx: &mpsc::Sender<NotificationAction>,
) -> bool {
    let tint = |color: Color32| color.gamma_multiply(opacity);
    let frame = Frame::new()
        .fill(tint(pal.panel))
        .stroke(Stroke::new(1.0, tint(pal.line)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 10))
        .shadow(Shadow {
            offset: [0, 2],
            blur: 8,
            spread: 0,
            color: Color32::from_black_alpha((48.0 * opacity) as u8),
        });
    let mut dismiss = false;
    let response = frame
        .show(ui, |ui| {
            let content_width = CARD_WIDTH - 28.0;
            ui.set_min_width(content_width);
            ui.set_max_width(content_width);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);
            if group.is_fused() {
                render_fused_group(ui, group, pal, opacity, action_tx, &mut dismiss);
            } else if let Some(entry) = group.entries.front() {
                render_single_group(ui, group, entry, pal, opacity, action_tx, &mut dismiss);
            }
        })
        .response
        .interact(egui::Sense::click());

    if response.clicked() && !dismiss {
        if let Some(action) = group.entries.front().and_then(|entry| entry.action.clone()) {
            let _ = action_tx.send(action);
            ui.ctx().request_repaint_of(ViewportId::ROOT);
            dismiss = true;
        }
    }
    dismiss
}

fn render_single_group(
    ui: &mut egui::Ui,
    group: &NotificationGroup,
    entry: &NotificationEntry,
    pal: &Palette,
    opacity: f32,
    action_tx: &mpsc::Sender<NotificationAction>,
    dismiss: &mut bool,
) {
    let tint = |color: Color32| color.gamma_multiply(opacity);
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            let indicator = match group.kind {
                NotificationKind::Error => pal.err,
                NotificationKind::Success => pal.ok,
                NotificationKind::IncomingCall | NotificationKind::Message => pal.accent,
                NotificationKind::Info => pal.dim,
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(6.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 3.0, tint(indicator));
            ui.label(
                RichText::new(group.kind.label())
                    .font(sans(9.0))
                    .strong()
                    .color(tint(pal.dim)),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if close_button(ui, pal, opacity).clicked() {
                    *dismiss = true;
                }
            });
        });
        ui.add_space(3.0);
        ui.add(
            egui::Label::new(
                RichText::new(&entry.title)
                    .size(ui_font_size(13.5))
                    .strong()
                    .color(tint(pal.text)),
            )
            .truncate(),
        );
        ui.add(
            egui::Label::new(
                RichText::new(&entry.body)
                    .size(ui_font_size(11.0))
                    .color(tint(pal.text2)),
            )
            .truncate(),
        );
        if !group.buttons.is_empty() {
            ui.add_space(7.0);
            ui.horizontal_top(|ui| {
                for button in &group.buttons {
                    let fill = if button.emphasized {
                        pal.accent
                    } else {
                        pal.panel2
                    };
                    let text = if button.emphasized { pal.bg } else { pal.text2 };
                    let response = ui.add_sized(
                        [92.0, 28.0],
                        egui::Button::new(
                            RichText::new(&button.label)
                                .size(ui_font_size(10.5))
                                .strong()
                                .color(tint(text)),
                        )
                        .fill(tint(fill))
                        .stroke(Stroke::new(1.0, tint(pal.line_br)))
                        .corner_radius(CornerRadius::same(7)),
                    );
                    if response.clicked() {
                        let _ = action_tx.send(button.action.clone());
                        ui.ctx().request_repaint_of(ViewportId::ROOT);
                        *dismiss = true;
                    }
                }
            });
        }
    });
}

fn render_fused_group(
    ui: &mut egui::Ui,
    group: &NotificationGroup,
    pal: &Palette,
    opacity: f32,
    action_tx: &mpsc::Sender<NotificationAction>,
    dismiss: &mut bool,
) {
    let tint = |color: Color32| color.gamma_multiply(opacity);
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} NEW MESSAGES",
                    group.entries.len() + group.hidden_entries
                ))
                .font(sans(9.0))
                .strong()
                .color(tint(pal.dim)),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if close_button(ui, pal, opacity).clicked() {
                    *dismiss = true;
                }
            });
        });
        for (index, entry) in group.entries.iter().enumerate() {
            if index > 0 {
                let bounds = ui.max_rect();
                ui.painter().hline(
                    bounds.left()..=bounds.right(),
                    ui.cursor().top(),
                    Stroke::new(1.0, tint(pal.line)),
                );
            }
            let response = ui
                .vertical(|ui| {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&entry.title)
                                .size(ui_font_size(11.5))
                                .strong()
                                .color(tint(pal.text)),
                        )
                        .truncate(),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(&entry.body)
                                .size(ui_font_size(10.5))
                                .color(tint(pal.text2)),
                        )
                        .truncate(),
                    );
                    ui.add_space(4.0);
                })
                .response
                .interact(egui::Sense::click());
            if response.clicked() {
                if let Some(action) = entry.action.clone() {
                    let _ = action_tx.send(action);
                    ui.ctx().request_repaint_of(ViewportId::ROOT);
                    *dismiss = true;
                }
            }
        }
        if group.hidden_entries > 0 {
            ui.label(
                RichText::new(format!("+{} earlier", group.hidden_entries))
                    .size(ui_font_size(10.0))
                    .color(tint(pal.dim)),
            );
        }
    });
}

fn close_button(ui: &mut egui::Ui, pal: &Palette, opacity: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), egui::Sense::click());
    let color = if response.hovered() {
        pal.text2
    } else {
        pal.dim
    }
    .gamma_multiply(opacity);
    let center = rect.center();
    let radius = 4.25;
    let stroke = Stroke::new(1.25, color);
    ui.painter().line_segment(
        [
            center + egui::vec2(-radius, -radius),
            center + egui::vec2(radius, radius),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            center + egui::vec2(-radius, radius),
            center + egui::vec2(radius, -radius),
        ],
        stroke,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_spec(key: &str, body: &str) -> NotificationSpec {
        NotificationSpec {
            group_key: Some(key.to_owned()),
            kind: NotificationKind::Message,
            title: "Maya".to_owned(),
            body: body.to_owned(),
            action: None,
            buttons: Vec::new(),
        }
    }

    #[test]
    fn messages_from_the_same_conversation_fuse() {
        let now = Instant::now();
        let mut store = NotificationStore::default();
        store.push(message_spec("conversation:one", "first"), now);
        store.push(
            message_spec("conversation:one", "second"),
            now + Duration::from_secs(1),
        );

        assert_eq!(store.visible.len(), 1);
        assert_eq!(store.visible[0].entries.len(), 2);
        assert_eq!(store.visible[0].entries[0].body, "second");
    }

    #[test]
    fn overflow_is_queued_and_promoted_after_dismissal() {
        let now = Instant::now();
        let mut store = NotificationStore::default();
        for index in 0..=MAX_VISIBLE {
            store.push(message_spec(&format!("conversation:{index}"), "hello"), now);
        }
        assert_eq!(store.visible.len(), MAX_VISIBLE);
        assert_eq!(store.pending.len(), 1);

        let id = store.visible[0].id;
        store.dismiss_id(id, now);
        store.advance(now + LEAVE_TIME);
        assert_eq!(store.visible.len(), MAX_VISIBLE);
        assert!(store.pending.is_empty());
    }

    #[test]
    fn critical_notifications_do_not_expire() {
        assert_eq!(NotificationKind::IncomingCall.default_lifetime(), None);
        assert!(NotificationKind::Error.default_lifetime().is_some());
    }

    #[test]
    fn overlay_can_render_in_an_embedded_viewport() {
        let service = NotificationService::default();
        service.info("preview", "Wire", "Notification preview");
        let context = egui::Context::default();
        context.set_embed_viewports(true);
        let _ = context.run(egui::RawInput::default(), |ctx| {
            service.show(ctx, Theme::Amber);
        });
    }
}
