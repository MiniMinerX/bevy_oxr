use bevy_log::error;
use bevy_math::UVec2;
use openxr::sys;
use openxr::sys::Handle as _;
use wgpu::{ExperimentalFeatures, InstanceFlags, MemoryBudgetThresholds, MemoryHints, BackendOptions};
use wgpu_hal::{Adapter, Instance};
use winapi::shared::dxgiformat::DXGI_FORMAT;
use winapi::um::d3d12 as winapi_d3d12;
use winapi::um::d3dcommon::D3D_FEATURE_LEVEL;

use super::{GraphicsExt, GraphicsType, GraphicsWrap, OxrManualGraphicsConfig};
use crate::error::OxrError;
use crate::session::OxrSessionCreateNextChain;
use crate::types::{AppInfo, OxrExtensions, Result, WgpuGraphics};

unsafe impl GraphicsExt for openxr::D3D12 {
    fn wrap<T: GraphicsType>(item: T::Inner<Self>) -> GraphicsWrap<T> {
        GraphicsWrap::D3D12(item)
    }

    fn required_exts() -> OxrExtensions {
        let mut extensions = openxr::ExtensionSet::default();
        extensions.khr_d3d12_enable = true;
        extensions.into()
    }

    fn from_wgpu_format(format: wgpu::TextureFormat) -> Option<Self::Format> {
        wgpu_to_d3d12(format)
    }

    fn into_wgpu_format(format: Self::Format) -> Option<wgpu::TextureFormat> {
        d3d12_to_wgpu(format)
    }

    unsafe fn to_wgpu_img(
        image: Self::SwapchainImage,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        resolution: UVec2,
    ) -> Result<wgpu::Texture> {
        let hal_dev = device.as_hal::<wgpu_hal::dx12::Api>().ok_or(
            OxrError::GraphicsBackendMismatch {
                item: "Wgpu Device",
                backend: "unknown",
                expected_backend: "d3d12",
            },
        )?;
        // D3D12 texture_from_raw expects windows::Win32::Graphics::Direct3D12::ID3D12Resource
        // OpenXR provides a raw pointer, convert it to the windows crate type
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
        use windows_core::Interface;
        let d3d12_resource = unsafe {
            ID3D12Resource::from_raw(image as *mut _)
        };
        let wgpu_hal_texture = unsafe {
            <wgpu_hal::dx12::Api as wgpu_hal::Api>::Device::texture_from_raw(
                d3d12_resource,
                format,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width: resolution.x,
                    height: resolution.y,
                    depth_or_array_layers: 2,
                },
                1, // mip_level_count
                1, // sample_count
            )
        };
        let texture = device.create_texture_from_hal::<wgpu_hal::dx12::Api>(
            wgpu_hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("VR Swapchain"),
                size: wgpu::Extent3d {
                    width: resolution.x,
                    height: resolution.y,
                    depth_or_array_layers: 2,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
        );
        Ok(texture)
    }

    fn init_graphics(
        app_info: &AppInfo,
        instance: &openxr::Instance,
        system_id: openxr::SystemId,
        _cfg: Option<&OxrManualGraphicsConfig>,
    ) -> Result<(WgpuGraphics, Self::SessionCreateInfo)> {
        let reqs = instance.graphics_requirements::<openxr::D3D12>(system_id)?;

        let instance_descriptor = wgpu_hal::InstanceDescriptor {
            name: &app_info.name,
            flags: InstanceFlags::from_build_config().with_env(),
            memory_budget_thresholds: MemoryBudgetThresholds::default(),
            backend_options: BackendOptions::default(),
        };
        let wgpu_raw_instance: wgpu_hal::dx12::Instance =
            unsafe { wgpu_hal::dx12::Instance::init(&instance_descriptor)? };
        let wgpu_adapters: Vec<wgpu_hal::ExposedAdapter<wgpu_hal::dx12::Api>> =
            unsafe { wgpu_raw_instance.enumerate_adapters(None) };

        let wgpu_exposed_adapter = wgpu_adapters
            .into_iter()
            .find(|a| {
                let desc = unsafe { a.adapter.raw_adapter().GetDesc1() }.unwrap();
                desc.AdapterLuid.HighPart == reqs.adapter_luid.HighPart
                    && desc.AdapterLuid.LowPart == reqs.adapter_luid.LowPart
            })
            .ok_or(OxrError::InitError(
                crate::error::InitError::FailedToFindD3D12Adapter,
            ))?;

        let wgpu_instance =
            unsafe { wgpu::Instance::from_hal::<wgpu_hal::api::Dx12>(wgpu_raw_instance) };

        let wgpu_features = wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
            | wgpu::Features::MULTIVIEW
            | wgpu::Features::MULTI_DRAW_INDIRECT_COUNT;

        let wgpu_limits = wgpu_exposed_adapter.capabilities.limits.clone();

        let wgpu_open_device = unsafe {
            wgpu_exposed_adapter
                .adapter
                .open(wgpu_features, &wgpu_limits, &MemoryHints::Performance)?
        };

        // Check feature level - wgpu-hal returns windows crate types, we need to get the raw COM pointer
        let raw_device_windows = wgpu_open_device.device.raw_device();
        // Get the raw COM interface pointer - windows crate types implement Interface trait with as_raw()
        #[cfg(feature = "d3d12")]
        use windows_core::Interface;
        let raw_device_ptr = raw_device_windows.as_raw();
        let raw_device_check = raw_device_ptr as *mut winapi_d3d12::ID3D12Device;
        let device_supported_feature_level = get_device_feature_level(raw_device_check);

        if (device_supported_feature_level as u32) < (reqs.min_feature_level as u32) {
            error!(
                "OpenXR runtime requires D3D12 feature level >= {}",
                reqs.min_feature_level
            );
            return Err(OxrError::FailedGraphicsRequirements);
        }

        let wgpu_adapter = unsafe { wgpu_instance.create_adapter_from_hal(wgpu_exposed_adapter) };
        // Extract raw pointers for OpenXR - get COM interface pointers  
        let raw_queue_windows = wgpu_open_device.device.raw_queue();
        let raw_queue_ptr = raw_queue_windows.as_raw();
        let raw_queue = raw_queue_ptr as *mut winapi_d3d12::ID3D12CommandQueue;
        // Reuse the device pointer we already extracted
        let raw_device = raw_device_ptr as *mut winapi_d3d12::ID3D12Device;
        let (wgpu_device, wgpu_queue) = unsafe {
            wgpu_adapter.create_device_from_hal(
                wgpu_open_device,
                &wgpu::DeviceDescriptor {
                    label: Some("bevy_oxr device"),
                    required_features: wgpu_features,
                    required_limits: wgpu_limits,
                    memory_hints: MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                    experimental_features: ExperimentalFeatures::enabled(),
                },
            )?
        };

        Ok((
            WgpuGraphics(
                wgpu_device,
                wgpu_queue,
                wgpu_adapter.get_info(),
                wgpu_adapter,
                wgpu_instance,
            ),
            Self::SessionCreateInfo {
                device: raw_device.cast(),
                queue: raw_queue.cast(),
            },
        ))
    }

    unsafe fn create_session(
        instance: &openxr::Instance,
        system_id: openxr::SystemId,
        info: &Self::SessionCreateInfo,
        session_create_info_chain: &mut OxrSessionCreateNextChain,
    ) -> openxr::Result<(
        openxr::Session<Self>,
        openxr::FrameWaiter,
        openxr::FrameStream<Self>,
    )> {
        let binding = sys::GraphicsBindingD3D12KHR {
            ty: sys::GraphicsBindingD3D12KHR::TYPE,
            next: session_create_info_chain.chain_pointer(),
            device: info.device,
            queue: info.queue,
        };
        let info = sys::SessionCreateInfo {
            ty: sys::SessionCreateInfo::TYPE,
            next: &binding as *const _ as *const _,
            create_flags: Default::default(),
            system_id: system_id,
        };
        let mut out = sys::Session::NULL;
          unsafe {
            cvt((instance.fp().create_session)(
                instance.as_raw(),
                &info,
                &mut out,
            ))?;
            Ok(openxr::Session::from_raw(
                instance.clone(),
                out,
                Box::new(()),
            ))
        }
    }

    fn init_fallback_graphics(
        app_info: &AppInfo,
        _cfg: &OxrManualGraphicsConfig,
    ) -> Result<WgpuGraphics> {
        let instance_descriptor = wgpu_hal::InstanceDescriptor {
            name: &app_info.name,
            flags: InstanceFlags::from_build_config().with_env(),
            memory_budget_thresholds: MemoryBudgetThresholds::default(),
            backend_options: BackendOptions::default(),
        };
        let wgpu_raw_instance: wgpu_hal::dx12::Instance =
            unsafe { wgpu_hal::dx12::Instance::init(&instance_descriptor)? };
        let wgpu_adapters: Vec<wgpu_hal::ExposedAdapter<wgpu_hal::dx12::Api>> =
            unsafe { wgpu_raw_instance.enumerate_adapters(None) };

        let wgpu_exposed_adapter = wgpu_adapters
            .into_iter()
            .next()
            .ok_or(OxrError::NoAvailableBackend)?;

        let wgpu_instance =
            unsafe { wgpu::Instance::from_hal::<wgpu_hal::api::Dx12>(wgpu_raw_instance) };

        let wgpu_features = wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
            | wgpu::Features::MULTIVIEW
            | wgpu::Features::MULTI_DRAW_INDIRECT_COUNT;

        let wgpu_limits = wgpu_exposed_adapter.capabilities.limits.clone();

        let wgpu_open_device = unsafe {
            wgpu_exposed_adapter
                .adapter
                .open(wgpu_features, &wgpu_limits, &MemoryHints::Performance)?
        };

        let wgpu_adapter = unsafe { wgpu_instance.create_adapter_from_hal(wgpu_exposed_adapter) };
        let (wgpu_device, wgpu_queue) = unsafe {
            wgpu_adapter.create_device_from_hal(
                wgpu_open_device,
                &wgpu::DeviceDescriptor {
                    label: Some("bevy_oxr fallback device"),
                    required_features: wgpu_features,
                    required_limits: wgpu_limits,
                    memory_hints: MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                    experimental_features: ExperimentalFeatures::enabled(),
                },
            )?
        };

        Ok(WgpuGraphics(
            wgpu_device,
            wgpu_queue,
            wgpu_adapter.get_info(),
            wgpu_adapter,
            wgpu_instance,
        ))
    }
}

fn cvt(x: sys::Result) -> openxr::Result<sys::Result> {
    if x.into_raw() >= 0 {
        Ok(x)
    } else {
        Err(x)
    }
}

// Check device feature level using winapi directly
fn get_device_feature_level(
    device: *mut winapi_d3d12::ID3D12Device,
) -> D3D_FEATURE_LEVEL {
    use winapi::um::d3dcommon::*;
    // Detect the highest supported feature level.
    let d3d_feature_level = [
        D3D_FEATURE_LEVEL_12_1,
        D3D_FEATURE_LEVEL_12_0,
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    ];
    type FeatureLevelsInfo = winapi_d3d12::D3D12_FEATURE_DATA_FEATURE_LEVELS;
    let mut device_levels: FeatureLevelsInfo = unsafe { std::mem::zeroed() };
    device_levels.NumFeatureLevels = d3d_feature_level.len() as u32;
    device_levels.pFeatureLevelsRequested = d3d_feature_level.as_ptr().cast();
    unsafe {
        (*device).CheckFeatureSupport(
            winapi_d3d12::D3D12_FEATURE_FEATURE_LEVELS,
            (&mut device_levels as *mut FeatureLevelsInfo).cast(),
            std::mem::size_of::<FeatureLevelsInfo>() as _,
        )
    };
    device_levels.MaxSupportedFeatureLevel
}

fn d3d12_to_wgpu(format: DXGI_FORMAT) -> Option<wgpu::TextureFormat> {
    use wgpu::TextureFormat as Tf;
    use winapi::shared::dxgiformat::*;

    Some(match format {
        DXGI_FORMAT_R8_UNORM => Tf::R8Unorm,
        DXGI_FORMAT_R8_SNORM => Tf::R8Snorm,
        DXGI_FORMAT_R8_UINT => Tf::R8Uint,
        DXGI_FORMAT_R8_SINT => Tf::R8Sint,
        DXGI_FORMAT_R16_UINT => Tf::R16Uint,
        DXGI_FORMAT_R16_SINT => Tf::R16Sint,
        DXGI_FORMAT_R16_UNORM => Tf::R16Unorm,
        DXGI_FORMAT_R16_SNORM => Tf::R16Snorm,
        DXGI_FORMAT_R16_FLOAT => Tf::R16Float,
        DXGI_FORMAT_R8G8_UNORM => Tf::Rg8Unorm,
        DXGI_FORMAT_R8G8_SNORM => Tf::Rg8Snorm,
        DXGI_FORMAT_R8G8_UINT => Tf::Rg8Uint,
        DXGI_FORMAT_R8G8_SINT => Tf::Rg8Sint,
        DXGI_FORMAT_R16G16_UNORM => Tf::Rg16Unorm,
        DXGI_FORMAT_R16G16_SNORM => Tf::Rg16Snorm,
        DXGI_FORMAT_R32_UINT => Tf::R32Uint,
        DXGI_FORMAT_R32_SINT => Tf::R32Sint,
        DXGI_FORMAT_R32_FLOAT => Tf::R32Float,
        DXGI_FORMAT_R16G16_UINT => Tf::Rg16Uint,
        DXGI_FORMAT_R16G16_SINT => Tf::Rg16Sint,
        DXGI_FORMAT_R16G16_FLOAT => Tf::Rg16Float,
        DXGI_FORMAT_R8G8B8A8_UNORM => Tf::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Tf::Rgba8UnormSrgb,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Tf::Bgra8UnormSrgb,
        DXGI_FORMAT_R8G8B8A8_SNORM => Tf::Rgba8Snorm,
        DXGI_FORMAT_B8G8R8A8_UNORM => Tf::Bgra8Unorm,
        DXGI_FORMAT_R8G8B8A8_UINT => Tf::Rgba8Uint,
        DXGI_FORMAT_R8G8B8A8_SINT => Tf::Rgba8Sint,
        DXGI_FORMAT_R9G9B9E5_SHAREDEXP => Tf::Rgb9e5Ufloat,
        DXGI_FORMAT_R10G10B10A2_UINT => Tf::Rgb10a2Uint,
        DXGI_FORMAT_R10G10B10A2_UNORM => Tf::Rgb10a2Unorm,
        DXGI_FORMAT_R11G11B10_FLOAT => Tf::Rg11b10Ufloat,
        DXGI_FORMAT_R32G32_UINT => Tf::Rg32Uint,
        DXGI_FORMAT_R32G32_SINT => Tf::Rg32Sint,
        DXGI_FORMAT_R32G32_FLOAT => Tf::Rg32Float,
        DXGI_FORMAT_R16G16B16A16_UINT => Tf::Rgba16Uint,
        DXGI_FORMAT_R16G16B16A16_SINT => Tf::Rgba16Sint,
        DXGI_FORMAT_R16G16B16A16_UNORM => Tf::Rgba16Unorm,
        DXGI_FORMAT_R16G16B16A16_SNORM => Tf::Rgba16Snorm,
        DXGI_FORMAT_R16G16B16A16_FLOAT => Tf::Rgba16Float,
        DXGI_FORMAT_R32G32B32A32_UINT => Tf::Rgba32Uint,
        DXGI_FORMAT_R32G32B32A32_SINT => Tf::Rgba32Sint,
        DXGI_FORMAT_R32G32B32A32_FLOAT => Tf::Rgba32Float,
        DXGI_FORMAT_D24_UNORM_S8_UINT => Tf::Stencil8,
        DXGI_FORMAT_D16_UNORM => Tf::Depth16Unorm,
        DXGI_FORMAT_D32_FLOAT => Tf::Depth32Float,
        DXGI_FORMAT_D32_FLOAT_S8X24_UINT => Tf::Depth32FloatStencil8,
        DXGI_FORMAT_NV12 => Tf::NV12,
        DXGI_FORMAT_BC1_UNORM => Tf::Bc1RgbaUnorm,
        DXGI_FORMAT_BC1_UNORM_SRGB => Tf::Bc1RgbaUnormSrgb,
        DXGI_FORMAT_BC2_UNORM => Tf::Bc2RgbaUnorm,
        DXGI_FORMAT_BC2_UNORM_SRGB => Tf::Bc2RgbaUnormSrgb,
        DXGI_FORMAT_BC3_UNORM => Tf::Bc3RgbaUnorm,
        DXGI_FORMAT_BC3_UNORM_SRGB => Tf::Bc3RgbaUnormSrgb,
        DXGI_FORMAT_BC4_UNORM => Tf::Bc4RUnorm,
        DXGI_FORMAT_BC4_SNORM => Tf::Bc4RSnorm,
        DXGI_FORMAT_BC5_UNORM => Tf::Bc5RgUnorm,
        DXGI_FORMAT_BC5_SNORM => Tf::Bc5RgSnorm,
        DXGI_FORMAT_BC6H_UF16 => Tf::Bc6hRgbUfloat,
        DXGI_FORMAT_BC6H_SF16 => Tf::Bc6hRgbFloat,
        DXGI_FORMAT_BC7_UNORM => Tf::Bc7RgbaUnorm,
        DXGI_FORMAT_BC7_UNORM_SRGB => Tf::Bc7RgbaUnormSrgb,
        _ => return None,
    })
}

fn wgpu_to_d3d12(format: wgpu::TextureFormat) -> Option<DXGI_FORMAT> {
    // Copied wholesale from:
    // https://github.com/gfx-rs/wgpu/blob/v0.19/wgpu-hal/src/auxil/dxgi/conv.rs#L12-L94
    // license: MIT OR Apache-2.0
    use wgpu::TextureFormat as Tf;
    use winapi::shared::dxgiformat::*;

    Some(match format {
        Tf::R8Unorm => DXGI_FORMAT_R8_UNORM,
        Tf::R8Snorm => DXGI_FORMAT_R8_SNORM,
        Tf::R8Uint => DXGI_FORMAT_R8_UINT,
        Tf::R8Sint => DXGI_FORMAT_R8_SINT,
        Tf::R16Uint => DXGI_FORMAT_R16_UINT,
        Tf::R16Sint => DXGI_FORMAT_R16_SINT,
        Tf::R16Unorm => DXGI_FORMAT_R16_UNORM,
        Tf::R16Snorm => DXGI_FORMAT_R16_SNORM,
        Tf::R16Float => DXGI_FORMAT_R16_FLOAT,
        Tf::Rg8Unorm => DXGI_FORMAT_R8G8_UNORM,
        Tf::Rg8Snorm => DXGI_FORMAT_R8G8_SNORM,
        Tf::Rg8Uint => DXGI_FORMAT_R8G8_UINT,
        Tf::Rg8Sint => DXGI_FORMAT_R8G8_SINT,
        Tf::Rg16Unorm => DXGI_FORMAT_R16G16_UNORM,
        Tf::Rg16Snorm => DXGI_FORMAT_R16G16_SNORM,
        Tf::R32Uint => DXGI_FORMAT_R32_UINT,
        Tf::R32Sint => DXGI_FORMAT_R32_SINT,
        Tf::R32Float => DXGI_FORMAT_R32_FLOAT,
        Tf::Rg16Uint => DXGI_FORMAT_R16G16_UINT,
        Tf::Rg16Sint => DXGI_FORMAT_R16G16_SINT,
        Tf::Rg16Float => DXGI_FORMAT_R16G16_FLOAT,
        Tf::Rgba8Unorm => DXGI_FORMAT_R8G8B8A8_UNORM,
        Tf::Rgba8UnormSrgb => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        Tf::Bgra8UnormSrgb => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        Tf::Rgba8Snorm => DXGI_FORMAT_R8G8B8A8_SNORM,
        Tf::Bgra8Unorm => DXGI_FORMAT_B8G8R8A8_UNORM,
        Tf::Rgba8Uint => DXGI_FORMAT_R8G8B8A8_UINT,
        Tf::Rgba8Sint => DXGI_FORMAT_R8G8B8A8_SINT,
        Tf::Rgb9e5Ufloat => DXGI_FORMAT_R9G9B9E5_SHAREDEXP,
        Tf::Rgb10a2Uint => DXGI_FORMAT_R10G10B10A2_UINT,
        Tf::Rgb10a2Unorm => DXGI_FORMAT_R10G10B10A2_UNORM,
        Tf::Rg11b10Ufloat => DXGI_FORMAT_R11G11B10_FLOAT,
        Tf::Rg32Uint => DXGI_FORMAT_R32G32_UINT,
        Tf::Rg32Sint => DXGI_FORMAT_R32G32_SINT,
        Tf::Rg32Float => DXGI_FORMAT_R32G32_FLOAT,
        Tf::Rgba16Uint => DXGI_FORMAT_R16G16B16A16_UINT,
        Tf::Rgba16Sint => DXGI_FORMAT_R16G16B16A16_SINT,
        Tf::Rgba16Unorm => DXGI_FORMAT_R16G16B16A16_UNORM,
        Tf::Rgba16Snorm => DXGI_FORMAT_R16G16B16A16_SNORM,
        Tf::Rgba16Float => DXGI_FORMAT_R16G16B16A16_FLOAT,
        Tf::Rgba32Uint => DXGI_FORMAT_R32G32B32A32_UINT,
        Tf::Rgba32Sint => DXGI_FORMAT_R32G32B32A32_SINT,
        Tf::Rgba32Float => DXGI_FORMAT_R32G32B32A32_FLOAT,
        Tf::Stencil8 => DXGI_FORMAT_D24_UNORM_S8_UINT,
        Tf::Depth16Unorm => DXGI_FORMAT_D16_UNORM,
        Tf::Depth24Plus => DXGI_FORMAT_D24_UNORM_S8_UINT,
        Tf::Depth24PlusStencil8 => DXGI_FORMAT_D24_UNORM_S8_UINT,
        Tf::Depth32Float => DXGI_FORMAT_D32_FLOAT,
        Tf::Depth32FloatStencil8 => DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        Tf::NV12 => DXGI_FORMAT_NV12,
        Tf::Bc1RgbaUnorm => DXGI_FORMAT_BC1_UNORM,
        Tf::Bc1RgbaUnormSrgb => DXGI_FORMAT_BC1_UNORM_SRGB,
        Tf::Bc2RgbaUnorm => DXGI_FORMAT_BC2_UNORM,
        Tf::Bc2RgbaUnormSrgb => DXGI_FORMAT_BC2_UNORM_SRGB,
        Tf::Bc3RgbaUnorm => DXGI_FORMAT_BC3_UNORM,
        Tf::Bc3RgbaUnormSrgb => DXGI_FORMAT_BC3_UNORM_SRGB,
        Tf::Bc4RUnorm => DXGI_FORMAT_BC4_UNORM,
        Tf::Bc4RSnorm => DXGI_FORMAT_BC4_SNORM,
        Tf::Bc5RgUnorm => DXGI_FORMAT_BC5_UNORM,
        Tf::Bc5RgSnorm => DXGI_FORMAT_BC5_SNORM,
        Tf::Bc6hRgbUfloat => DXGI_FORMAT_BC6H_UF16,
        Tf::Bc6hRgbFloat => DXGI_FORMAT_BC6H_SF16,
        Tf::Bc7RgbaUnorm => DXGI_FORMAT_BC7_UNORM,
        Tf::Bc7RgbaUnormSrgb => DXGI_FORMAT_BC7_UNORM_SRGB,
        Tf::Etc2Rgb8Unorm
        | Tf::Etc2Rgb8UnormSrgb
        | Tf::Etc2Rgb8A1Unorm
        | Tf::Etc2Rgb8A1UnormSrgb
        | Tf::Etc2Rgba8Unorm
        | Tf::Etc2Rgba8UnormSrgb
        | Tf::EacR11Unorm
        | Tf::EacR11Snorm
        | Tf::EacRg11Unorm
        | Tf::EacRg11Snorm
        | Tf::Astc {
            block: _,
            channel: _,
        }
        | Tf::R64Uint
        | Tf::P010 => return None,
    })
}

