//! Dev-only visual snapshot harness.
//!
//! `WIRE_UI_CAPTURE=<dir>` loads in-memory fixture conversations, walks a
//! fixed list of scenes (window sizes, modes, themes, dialogs), saves one PNG
//! per scene through egui's own screenshot command, and quits. It never
//! touches the worker's chat documents, so pair it with a throwaway
//! `WIRE_CONFIG_DIR`:
//!
//! ```sh
//! scripts/ui-capture.sh /tmp/wire-shots            # all scenes
//! scripts/ui-capture.sh /tmp/wire-shots calls-     # filtered
//! ```
//!
//! Optional `WIRE_UI_CAPTURE_FILTER=<substring>` limits the scenes captured.

use super::{AppMode, AppState, ChatStyle, Friend};
use crate::{
    chat::{self, ChatConversation, ChatMessage, ConversationKind},
    client_status::{Availability, GroupCallAnnouncement, StatusUpdate},
    profile::{PeerProfile, ProfileSnapshot},
    runtime::CallState,
    theme::Theme,
};
use egui::Vec2;
use iroh::NodeId;
use std::{path::PathBuf, sync::Arc};
use tracing::{info, warn};

pub(super) const ENV: &str = "WIRE_UI_CAPTURE";
const FILTER_ENV: &str = "WIRE_UI_CAPTURE_FILTER";
const SETTLE_FRAMES: u32 = 14;
const MAX_SETTLE_FRAMES: u32 = 160;

#[derive(Clone, Copy)]
struct Scene {
    name: &'static str,
    size: [f32; 2],
    apply: fn(&mut AppState),
}

enum Phase {
    WaitingForIdentity { frames: u32 },
    Settling { frames: u32 },
    AwaitingShot,
    Done,
}

pub(super) struct UiCapture {
    dir: PathBuf,
    scenes: Vec<Scene>,
    index: usize,
    phase: Phase,
}

impl UiCapture {
    pub(super) fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os(ENV)?);
        let filter = std::env::var(FILTER_ENV).ok();
        let scenes = SCENES
            .iter()
            .copied()
            .filter(|scene| filter.as_deref().is_none_or(|f| scene.name.contains(f)))
            .collect();
        if let Err(error) = std::fs::create_dir_all(&dir) {
            warn!("could not create capture dir {}: {error}", dir.display());
            return None;
        }
        Some(Self {
            dir,
            scenes,
            index: 0,
            phase: Phase::WaitingForIdentity { frames: 0 },
        })
    }
}

impl AppState {
    /// Advance the capture harness by one frame. Runs before the UI is drawn.
    pub(super) fn drive_ui_capture(&mut self, ctx: &egui::Context) {
        let Some(mut capture) = self.ui_capture.take() else {
            return;
        };
        ctx.request_repaint();
        capture.phase = match capture.phase {
            Phase::WaitingForIdentity { frames } => {
                if self.our_node_id.is_some() || frames > 600 {
                    self.load_capture_fixtures();
                    self.start_scene(ctx, &capture);
                    Phase::Settling { frames: 0 }
                } else {
                    Phase::WaitingForIdentity { frames: frames + 1 }
                }
            }
            Phase::Settling { frames } => {
                let scene = capture.scenes[capture.index];
                // Re-apply each frame so worker events cannot undo the scene.
                (scene.apply)(self);
                let target = Vec2::from(scene.size);
                let size = ctx.content_rect().size();
                let settled = (size - target).length() < 2.0;
                if frames >= SETTLE_FRAMES && (settled || frames >= MAX_SETTLE_FRAMES) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                        capture.index,
                    )));
                    Phase::AwaitingShot
                } else {
                    Phase::Settling { frames: frames + 1 }
                }
            }
            Phase::AwaitingShot => {
                let image = ctx.input(|input| {
                    input.raw.events.iter().find_map(|event| match event {
                        egui::Event::Screenshot { image, .. } => Some(image.clone()),
                        _ => None,
                    })
                });
                match image {
                    Some(image) => {
                        let scene = capture.scenes[capture.index];
                        save_png(&capture.dir, scene.name, &image);
                        capture.index += 1;
                        if capture.index >= capture.scenes.len() {
                            info!("ui capture finished");
                            self.exit_requested = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            Phase::Done
                        } else {
                            self.start_scene(ctx, &capture);
                            Phase::Settling { frames: 0 }
                        }
                    }
                    None => Phase::AwaitingShot,
                }
            }
            Phase::Done => Phase::Done,
        };
        self.ui_capture = Some(capture);
    }

    fn start_scene(&mut self, ctx: &egui::Context, capture: &UiCapture) {
        let Some(scene) = capture.scenes.get(capture.index).copied() else {
            return;
        };
        reset_scene_state(self);
        (scene.apply)(self);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::from(scene.size)));
    }

    fn load_capture_fixtures(&mut self) {
        let ours = self.our_node_id.unwrap_or_else(|| fixture_peer(250));
        self.our_node_id = Some(ours);
        self.configured = true;
        self.show_settings = false;
        self.own_profile_name = "Noah".to_owned();
        self.own_accent_color = Some("#d99a5b".to_owned());

        let people = [
            (1, "Mira", Some("#c58fd8"), true),
            (2, "David", Some("#7fb0e8"), false),
            (3, "Jules", None, true),
            (4, "Alexandria Montgomery-Hughes", Some("#8fd8a8"), false),
        ];
        let now = chat::now_millis();
        for (seed, name, accent, online) in people {
            let peer = fixture_peer(seed);
            self.friends.push(Friend {
                name: name.to_owned(),
                node_id: peer.to_string(),
            });
            self.peer_profiles.insert(
                peer,
                PeerProfile {
                    display_name: Some(name.to_owned()),
                    avatar_hash: None,
                    accent_color: accent.map(str::to_owned),
                    last_seen_ms: Some(now),
                },
            );
            self.friend_status.insert(
                peer,
                StatusUpdate {
                    peer,
                    availability: if online {
                        Availability::Online
                    } else {
                        Availability::Offline
                    },
                    client_version: None,
                    active_group_calls: Vec::new(),
                    profile: ProfileSnapshot::default(),
                },
            );
        }

        let mira = fixture_peer(1);
        let david = fixture_peer(2);
        let jules = fixture_peer(3);
        let direct = |peer: NodeId| ChatConversation {
            id: chat::direct_conversation_id(ours, peer),
            title: String::new(),
            kind: ConversationKind::Direct {
                peer_id: peer.to_string(),
            },
            members: vec![ours.to_string(), peer.to_string()],
            document_id: String::new(),
            history_epoch: 0,
        };
        let message = |author: NodeId, minutes_ago: i64, body: &str| {
            let mut message = ChatMessage::new_with_attachments(author, body.to_owned(), vec![]);
            message.sent_at = now - minutes_ago * 60_000;
            message
        };

        let mira_chat = direct(mira);
        let mira_id = mira_chat.id.clone();
        self.chat.conversations.insert(mira_id.clone(), mira_chat);
        self.chat.timelines.insert(
            mira_id.clone(),
            vec![
                message(mira, 26 * 60, "Did the relay fix land? My build from last night still drops after a few minutes."),
                message(ours, 26 * 60 - 3, "Landed this morning. Pull main and it should hold."),
                message(mira, 42, "The new decoder path is stable now. GPU load is still way too high though."),
                message(mira, 41, "Like 40% on an idle stream, which feels wrong."),
                message(ours, 38, "Yeah, I want to isolate that before touching the actual stream UI."),
                message(ours, 38, "Probably the texture upload path. I'll profile it tonight."),
                message(mira, 35, "Makes sense. Want to look at the call layout after? The participant strip wraps strangely when the window is narrow and the dock overlaps the self card."),
                message(ours, 2, "Sure, send me a screenshot when you have one 👍"),
            ],
        );

        let david_chat = direct(david);
        let david_id = david_chat.id.clone();
        self.chat.conversations.insert(david_id.clone(), david_chat);
        self.chat.timelines.insert(
            david_id.clone(),
            vec![message(
                david,
                14,
                "pushed the tray fix, can you test on mac?",
            )],
        );
        self.chat.unseen.insert(david_id);

        let group_id = "fixture-group-wire-dev".to_owned();
        self.chat.conversations.insert(
            group_id.clone(),
            ChatConversation {
                id: group_id.clone(),
                title: "Wire dev".to_owned(),
                kind: ConversationKind::Group,
                members: [ours, mira, david, jules]
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                document_id: String::new(),
                history_epoch: 0,
            },
        );
        self.chat.timelines.insert(
            group_id.clone(),
            vec![
                message(jules, 90, "standup in 10?"),
                message(david, 88, "omw"),
                message(mira, 87, "yep"),
            ],
        );
        self.chat.conversations.insert(
            "fixture-group-game-night".to_owned(),
            ChatConversation {
                id: "fixture-group-game-night".to_owned(),
                title: "Game night with a deliberately long group title".to_owned(),
                kind: ConversationKind::Group,
                members: [ours, mira, jules]
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                document_id: String::new(),
                history_epoch: 0,
            },
        );
        self.chat.selected = Some(mira_id);
    }
}

fn fixture_peer(seed: u8) -> NodeId {
    iroh::SecretKey::from_bytes(&[seed; 32]).public()
}

fn save_png(dir: &std::path::Path, name: &str, image: &Arc<egui::ColorImage>) {
    let [width, height] = image.size;
    let path = dir.join(format!("{name}.png"));
    match image::RgbaImage::from_raw(width as u32, height as u32, image.as_raw().to_vec()) {
        Some(buffer) => match buffer.save(&path) {
            Ok(()) => info!("ui capture saved {}", path.display()),
            Err(error) => warn!("could not save {}: {error}", path.display()),
        },
        None => warn!("screenshot buffer for {name} had an unexpected size"),
    }
}

fn reset_scene_state(state: &mut AppState) {
    state.theme = Theme::Amber;
    state.chat_style = ChatStyle::Bubbles;
    state.app_mode = AppMode::Text;
    state.show_settings = false;
    state.show_contacts = false;
    state.show_profile_editor = false;
    state.chat.show_group_editor = false;
    state.calls.clear();
    state.local_group_call = None;
    state.chat.selected = state
        .our_node_id
        .map(|ours| chat::direct_conversation_id(ours, fixture_peer(1)));
}

fn select_group(state: &mut AppState) {
    state.chat.selected = Some("fixture-group-wire-dev".to_owned());
}

fn in_call(state: &mut AppState) {
    state.app_mode = AppMode::Calls;
    state.calls.insert(fixture_peer(1), CallState::Active);
    state.calls.insert(fixture_peer(3), CallState::Active);
    state.calls.insert(fixture_peer(2), CallState::Incoming);
    let ours = state
        .our_node_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    state.local_group_call = Some(GroupCallAnnouncement {
        call_id: "fixture-call".to_owned(),
        conversation_id: "fixture-group-wire-dev".to_owned(),
        title: "Wire dev".to_owned(),
        initiator: ours.clone(),
        started_at_ms: chat::now_millis() - 5 * 60_000,
        ended_at_ms: None,
        participants: vec![
            ours,
            fixture_peer(1).to_string(),
            fixture_peer(3).to_string(),
        ],
    });
}

const DEFAULT: [f32; 2] = [1100.0, 720.0];
const MEDIUM: [f32; 2] = [780.0, 600.0];
const MINIMUM: [f32; 2] = [460.0, 500.0];
const LARGE: [f32; 2] = [1600.0, 1000.0];

const SCENES: &[Scene] = &[
    Scene {
        name: "text-default",
        size: DEFAULT,
        apply: |_| {},
    },
    Scene {
        name: "text-compact",
        size: DEFAULT,
        apply: |s| s.chat_style = ChatStyle::Compact,
    },
    Scene {
        name: "text-group",
        size: DEFAULT,
        apply: select_group,
    },
    Scene {
        name: "text-medium",
        size: MEDIUM,
        apply: |_| {},
    },
    Scene {
        name: "text-minimum",
        size: MINIMUM,
        apply: |_| {},
    },
    Scene {
        name: "text-large",
        size: LARGE,
        apply: |_| {},
    },
    Scene {
        name: "text-empty",
        size: DEFAULT,
        apply: |s| s.chat.selected = None,
    },
    Scene {
        name: "theme-terminal",
        size: DEFAULT,
        apply: |s| s.theme = Theme::Terminal,
    },
    Scene {
        name: "theme-oled",
        size: DEFAULT,
        apply: |s| s.theme = Theme::DiscordOled,
    },
    Scene {
        name: "theme-slate",
        size: DEFAULT,
        apply: |s| s.theme = Theme::Slate,
    },
    Scene {
        name: "calls-idle",
        size: DEFAULT,
        apply: |s| s.app_mode = AppMode::Calls,
    },
    Scene {
        name: "calls-active",
        size: DEFAULT,
        apply: in_call,
    },
    Scene {
        name: "calls-active-medium",
        size: MEDIUM,
        apply: in_call,
    },
    Scene {
        name: "calls-active-minimum",
        size: MINIMUM,
        apply: in_call,
    },
    Scene {
        name: "calls-active-large",
        size: LARGE,
        apply: in_call,
    },
    Scene {
        name: "settings-default",
        size: DEFAULT,
        apply: |s| s.show_settings = true,
    },
    Scene {
        name: "settings-minimum",
        size: MINIMUM,
        apply: |s| s.show_settings = true,
    },
    Scene {
        name: "profile-editor",
        size: DEFAULT,
        apply: |s| s.show_profile_editor = true,
    },
    Scene {
        name: "group-editor",
        size: DEFAULT,
        apply: |s| s.chat.show_group_editor = true,
    },
];
