//! D3D11 device manager required by MF hardware video encoders.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_1;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_WRITE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ERROR_NOT_FOUND,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFTransform, IMFVideoSampleAllocatorEx, MFCreateDXGIDeviceManager,
    MFCreateVideoSampleAllocatorEx, MFT_MESSAGE_SET_D3D_MANAGER,
};

pub struct MfD3d {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
    reset_token: u32,
}

pub fn enumerate_adapters() -> Result<Vec<IDXGIAdapter1>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapters = Vec::new();
    for index in 0u32.. {
        match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapters.push(adapter),
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(adapters)
}

fn adapter_for_luid(luid: LUID) -> Result<IDXGIAdapter1> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for index in 0u32.. {
        match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => {
                let desc = unsafe { adapter.GetDesc1()? };
                if desc.AdapterLuid.LowPart == luid.LowPart
                    && desc.AdapterLuid.HighPart == luid.HighPart
                {
                    return Ok(adapter);
                }
            }
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no DXGI adapter matches encoder LUID")
}

impl MfD3d {
    pub fn try_new_with_adapter(adapter: &IDXGIAdapter1) -> Result<Self> {
        Self::create_device(Some(adapter))
    }

    pub fn try_new(adapter_luid: Option<LUID>) -> Result<Self> {
        let adapter = adapter_luid.map(adapter_for_luid).transpose()?;
        Self::create_device(adapter.as_ref())
    }

    fn create_device(adapter: Option<&IDXGIAdapter1>) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let feature_levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        unsafe {
            match adapter {
                Some(adapter) => D3D11CreateDevice(
                    adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ),
                None => D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ),
            }
            .context("D3D11CreateDevice failed")?;
        }
        let device = device.context("D3D11 device was null")?;
        let context = context.context("D3D11 context was null")?;
        unsafe {
            if let Ok(mt) = device.cast::<ID3D10Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let mut reset_token = 0u32;
        let mut manager = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
                .context("MFCreateDXGIDeviceManager failed")?;
        }
        let manager = manager.context("DXGI device manager was null")?;
        unsafe {
            manager
                .ResetDevice(&device, reset_token)
                .context("IMFDXGIDeviceManager::ResetDevice failed")?;
        }
        Ok(Self {
            device,
            context,
            manager,
            reset_token: reset_token,
        })
    }

    pub fn attach_to_transform(&self, transform: &IMFTransform) -> Result<()> {
        unsafe {
            transform
                .ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    Interface::as_raw(&self.manager) as usize,
                )
                .context("MFT_MESSAGE_SET_D3D_MANAGER failed")?;
        }
        Ok(())
    }
}

pub struct GpuNv12Frames {
    allocator: IMFVideoSampleAllocatorEx,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl GpuNv12Frames {
    pub fn new(d3d: &MfD3d, input_type: &IMFMediaType, width: u32, height: u32) -> Result<Self> {
        let allocator: IMFVideoSampleAllocatorEx = unsafe {
            let mut allocator: Option<windows::core::IUnknown> = None;
            MFCreateVideoSampleAllocatorEx(
                &IMFVideoSampleAllocatorEx::IID,
                &mut allocator as *mut _ as *mut *mut _,
            )?;
            allocator
                .context("MFCreateVideoSampleAllocatorEx returned null")?
                .cast()?
        };
        unsafe {
            allocator.SetDirectXManager(&d3d.manager)?;
            allocator.InitializeSampleAllocator(4, input_type)?;
        }

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe {
            d3d.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .context("CreateTexture2D (NV12 staging) failed")?;
        }

        Ok(Self {
            allocator,
            staging: staging.context("NV12 staging texture was null")?,
            width,
            height,
        })
    }

    fn upload_to_texture(
        d3d: &MfD3d,
        texture: &ID3D11Texture2D,
        staging: &ID3D11Texture2D,
        width: u32,
        height: u32,
        nv12: &[u8],
    ) -> Result<()> {
        let expected = (width * (height + height / 2)) as usize;
        if nv12.len() < expected {
            anyhow::bail!("NV12 buffer too small for GPU upload");
        }

        let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            d3d.context
                .Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .context("Map NV12 staging texture failed")?;
        }
        let pitch = mapped.RowPitch as usize;
        let w = width as usize;
        let h = height as usize;
        unsafe {
            let base = mapped.pData as *mut u8;
            for row in 0..h {
                std::ptr::copy_nonoverlapping(nv12.as_ptr().add(row * w), base.add(row * pitch), w);
            }
            let uv_src = h * w;
            for row in 0..h / 2 {
                std::ptr::copy_nonoverlapping(
                    nv12.as_ptr().add(uv_src + row * w),
                    base.add(h * pitch + row * pitch),
                    w,
                );
            }
            d3d.context.Unmap(staging, 0);
            d3d.context.CopyResource(texture, staging);
        }
        Ok(())
    }

    fn fill_nv12_buffer(
        buffer: &IMFMediaBuffer,
        nv12: &[u8],
        width: u32,
        height: u32,
    ) -> Result<()> {
        let expected = (width * (height + height / 2)) as usize;
        if nv12.len() < expected {
            anyhow::bail!("NV12 buffer too small for surface fill");
        }
        if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
            let mut ptr = std::ptr::null_mut();
            let mut pitch = 0i32;
            unsafe {
                buf2d
                    .Lock2D(&mut ptr, &mut pitch)
                    .context("Lock2D on encoder surface failed")?;
            }
            let pitch = pitch as usize;
            let w = width as usize;
            let h = height as usize;
            unsafe {
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(
                        nv12.as_ptr().add(row * w),
                        ptr.add(row * pitch),
                        w,
                    );
                }
                let uv_src = h * w;
                for row in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(
                        nv12.as_ptr().add(uv_src + row * w),
                        ptr.add(h * pitch + row * pitch),
                        w,
                    );
                }
                buf2d.Unlock2D()?;
            }
            return Ok(());
        }
        anyhow::bail!("encoder surface buffer is not IMF2DBuffer")
    }

    pub fn create_sample(
        &self,
        d3d: &MfD3d,
        nv12: &[u8],
        timestamp: i64,
        duration: i64,
    ) -> Result<IMFSample> {
        let sample = unsafe {
            self.allocator
                .AllocateSample()
                .context("AllocateSample failed")?
        };
        let buffer: IMFMediaBuffer = unsafe {
            sample
                .GetBufferByIndex(0)
                .context("encoder sample missing buffer")?
        };
        if Self::fill_nv12_buffer(&buffer, nv12, self.width, self.height).is_err() {
            let dxgi: IMFDXGIBuffer = buffer.cast().context("encoder sample not DXGI")?;
            let mut resource: Option<windows::core::IUnknown> = None;
            unsafe {
                dxgi.GetResource(
                    &ID3D11Texture2D::IID,
                    &mut resource as *mut _ as *mut *mut _,
                )
                .context("GetResource ID3D11Texture2D failed")?;
            }
            let texture: ID3D11Texture2D = resource
                .context("DXGI resource was null")?
                .cast()
                .context("DXGI resource was not ID3D11Texture2D")?;
            Self::upload_to_texture(d3d, &texture, &self.staging, self.width, self.height, nv12)?;
        }
        unsafe {
            buffer.SetCurrentLength(buffer.GetMaxLength()?)?;
            d3d.context.Flush();
            if timestamp >= 0 {
                sample.SetSampleTime(timestamp)?;
            }
            if duration > 0 {
                sample.SetSampleDuration(duration)?;
            }
        }
        Ok(sample)
    }
}
