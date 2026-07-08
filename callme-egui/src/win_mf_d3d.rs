//! D3D11 device manager required by MF hardware video encoders.

use anyhow::{Context, Result};
use windows::Win32::Foundation::HMODULE;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_1;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_WRITE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFSample, IMFTransform, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateSample, MFT_MESSAGE_SET_D3D_MANAGER, IMFDXGIDeviceManager,
};

pub struct MfD3d {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
    reset_token: u32,
}

impl MfD3d {
    pub fn try_new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let feature_levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice failed")?;
        }
        let device = device.context("D3D11 device was null")?;
        let context = context.context("D3D11 context was null")?;
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
    texture: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl GpuNv12Frames {
    pub fn new(d3d: &MfD3d, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            d3d.device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .context("CreateTexture2D (NV12 default) failed")?;
        }
        let mut staging_desc = desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE.0 as u32;
        let mut staging = None;
        unsafe {
            d3d.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .context("CreateTexture2D (NV12 staging) failed")?;
        }
        Ok(Self {
            texture: texture.context("NV12 texture was null")?,
            staging: staging.context("NV12 staging texture was null")?,
            width,
            height,
        })
    }

    pub fn upload(&self, d3d: &MfD3d, nv12: &[u8]) -> Result<()> {
        let expected = (self.width * (self.height + self.height / 2)) as usize;
        if nv12.len() < expected {
            anyhow::bail!("NV12 buffer too small for GPU upload");
        }

        let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            d3d.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .context("Map NV12 staging texture failed")?;
        }
        let pitch = mapped.RowPitch as usize;
        let width = self.width as usize;
        let height = self.height as usize;
        unsafe {
            let base = mapped.pData as *mut u8;
            for row in 0..height {
                std::ptr::copy_nonoverlapping(
                    nv12.as_ptr().add(row * width),
                    base.add(row * pitch),
                    width,
                );
            }
            let uv_src = height * width;
            let uv_rows = height / 2;
            for row in 0..uv_rows {
                std::ptr::copy_nonoverlapping(
                    nv12.as_ptr().add(uv_src + row * width),
                    base.add(height * pitch + row * pitch),
                    width,
                );
            }
            d3d.context.Unmap(&self.staging, 0);
            d3d.context
                .CopyResource(&self.texture, &self.staging);
        }
        Ok(())
    }

    pub fn create_sample(&self, timestamp: i64, duration: i64) -> Result<IMFSample> {
        let buffer: IMFMediaBuffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.texture, 0, false)
                .context("MFCreateDXGISurfaceBuffer failed")?
        };
        let sample = unsafe { MFCreateSample().context("MFCreateSample failed")? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp)?;
            sample.SetSampleDuration(duration)?;
        }
        Ok(sample)
    }
}