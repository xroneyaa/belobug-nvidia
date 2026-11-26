//! Depth textures that shaders also write as unordered-access images.
//!
//! The console allows both depth-stencil use and unordered-access writes on
//! one texture (for example shadow/Hi-Z passes on `D16_UNORM` / `D32_FLOAT`
//! and Grand Theft Auto VI's material classifier on 16-bit depth + stencil
//! textures). Host D3D12 forbids `ALLOW_UNORDERED_ACCESS` on depth resources.
//!
//! Each such texture gets writable colour twin(s) mirroring its dimensions,
//! mip count, and array slices:
//! * Single-plane depth formats (`R16_TYPELESS` / `R32_TYPELESS`) get a matching
//!   typeless colour twin synced both ways via `CopyResource`.
//! * Depth-stencil formats (`R24G8_TYPELESS` / `R32G8X24_TYPELESS`, including
//!   console `118..=120`) get an `R8_TYPELESS` twin for the stencil plane synced
//!   both ways through a staging `DEFAULT` buffer via `CopyTextureRegion` (requiring
//!   no `SV_StencilRef` support from the host GPU), plus an `R16_TYPELESS` or
//!   `R32_TYPELESS` twin for the depth plane copied back through a full-screen
//!   `SV_Depth` pass.

use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::host;
use crate::resource::HostTexture;

const ONE_PLANE: [u32; 2] = [39, 53];
const WITH_STENCIL: [u32; 2] = [19, 44];
const R8_TYPELESS: u32 = 60;
const UNORDERED_ACCESS: u32 = 0x8;
const PLANE_VIEWS: u32 = 512;

/// Writable colour twin(s) backing a depth texture's unordered-access views.
pub(crate) struct Twin {
    /// Colour twin for single-plane depth, or `R8_TYPELESS` stencil twin for
    /// depth-stencil textures.
    pub(crate) colour: Arc<HostTexture>,
    /// Cross-submission dirtiness bitmask: bit 0 = depth behind twin, bit 1 = twin behind depth.
    pub(crate) behind: AtomicU32,
    /// Intermediate `DEFAULT` buffer for `CopyTextureRegion` stencil transfers
    /// between depth Plane 1 and `R8_TYPELESS` `colour`.
    pub(crate) through: Option<ID3D12Resource>,
    /// Depth-plane twin (`R16_TYPELESS` for console `118..=120`, `R32_TYPELESS`
    /// otherwise) when the resource also has a stencil plane.
    pub(crate) plane: Option<Arc<HostTexture>>,
}

/// Whether a title texture of console `format` and `flags` gets colour twin(s).
/// `VICTORY_DEPTH_TWINS=0` disables twins; `VICTORY_DEPTH_TWINS=stencil` enables
/// only depth-stencil formats.
pub(crate) fn wanted(format: u32, flags: u32) -> bool {
    const DEPTH_AND_UNORDERED: u32 = 6;
    if flags & DEPTH_AND_UNORDERED != DEPTH_AND_UNORDERED {
        return false;
    }
    static OFF: OnceLock<bool> = OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("VICTORY_DEPTH_TWINS").is_some_and(|v| v == "0")) {
        return false;
    }
    static STENCIL_ONLY: OnceLock<bool> = OnceLock::new();
    let stencil_only = *STENCIL_ONLY
        .get_or_init(|| std::env::var_os("VICTORY_DEPTH_TWINS").is_some_and(|v| v == "stencil"));
    let typeless = crate::resource::family(crate::resource::pc_format(format));
    WITH_STENCIL.contains(&typeless) || (!stencil_only && ONE_PLANE.contains(&typeless))
}

fn make_twin_texture(
    host: &crate::host::Host,
    of: &D3D12_RESOURCE_DESC,
    format: u32,
    label: &str,
) -> Option<Arc<HostTexture>> {
    let desc = D3D12_RESOURCE_DESC {
        Format: DXGI_FORMAT(format as i32),
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        Alignment: 0,
        ..*of
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: valid description matching `depth`'s extent; out pointer is ours.
    let res = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            None,
            &mut texture,
        )
    };
    if let Err(e) = res {
        tracing::warn!(error = %e, format, label, "depth twin texture not made");
        return None;
    }
    let texture = texture?;
    crate::host::name(&texture, || {
        format!(
            "{label} {}x{}x{} mips {} f{format}",
            of.Width, of.Height, of.DepthOrArraySize, of.MipLevels
        )
    });
    Some(HostTexture::auxiliary(texture, UNORDERED_ACCESS))
}

/// Makes colour twin(s) for host depth texture `depth`.
pub(crate) fn make(title_format: u32, sample_count: u32, depth: &ID3D12Resource) -> Option<Twin> {
    if sample_count > 1 {
        return None;
    }
    let host = host()?;
    // SAFETY: live resource.
    let of = unsafe { depth.GetDesc() };
    if of.SampleDesc.Count > 1 {
        return None;
    }
    let typeless = crate::resource::family(crate::resource::pc_format(title_format));
    let has_stencil = WITH_STENCIL.contains(&typeless);
    if !has_stencil && !ONE_PLANE.contains(&typeless) {
        return None;
    }
    let colour = make_twin_texture(
        host,
        &of,
        if has_stencil { R8_TYPELESS } else { typeless },
        if has_stencil { "depth stencil twin" } else { "depth twin" },
    )?;
    let (through, plane) = if has_stencil {
        let planes = u32::from(of.MipLevels).max(1) * u32::from(of.DepthOrArraySize).max(1);
        let mut bytes = 0;
        // SAFETY: valid live resource query writing total byte size for Plane 1 subresources.
        unsafe {
            host.device
                .GetCopyableFootprints(&of, planes, planes, 0, None, None, None, Some(&mut bytes));
        }
        if bytes == 0 || bytes == u64::MAX {
            tracing::warn!(bytes, "depth stencil twin footprint invalid");
            return None;
        }
        let buffer = host.uav_buffer(bytes)?;
        crate::host::name(&buffer, || format!("depth stencil twin bridge {bytes}B"));
        static NO_PLANE: OnceLock<bool> = OnceLock::new();
        let no_plane = *NO_PLANE
            .get_or_init(|| std::env::var_os("VICTORY_DEPTH_TWINS").is_some_and(|v| v == "nodepth"));
        let plane = (!no_plane).then(|| {
            let fmt = if (118..=120).contains(&title_format) { 53 } else { 39 };
            make_twin_texture(host, &of, fmt, "depth plane twin")
        }).flatten();
        (Some(buffer), plane)
    } else {
        (None, None)
    };
    tracing::info!(
        width = of.Width,
        height = of.Height,
        slices = of.DepthOrArraySize,
        mips = of.MipLevels,
        title_format,
        stencil = through.is_some(),
        plane = plane.is_some(),
        "depth texture written as an image: twin made"
    );
    Some(Twin {
        colour,
        behind: AtomicU32::new(0),
        through,
        plane,
    })
}

fn uav_for(
    target: &ID3D12Resource,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    // SAFETY: live resource.
    let d = unsafe { target.GetDesc() };
    let mut desc = desc.cloned().unwrap_or_else(|| {
        let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        if d.DepthOrArraySize > 1 {
            all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: u32::from(d.DepthOrArraySize),
                ..Default::default()
            };
        }
        all
    });
    if d.Format.0 as u32 == R8_TYPELESS {
        let mut plane = 0;
        match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: matches `ViewDimension`.
                let mut view = unsafe { desc.Anonymous.Texture2D };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2D = view;
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: matches `ViewDimension`.
                let mut view = unsafe { desc.Anonymous.Texture2DArray };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2DArray = view;
            }
            _ => {}
        }
        if plane != 1 && !matches!(desc.Format.0, 22 | 47 | 62) {
            return None;
        }
        desc.Format = DXGI_FORMAT_R8_UINT;
        return Some(desc);
    }
    desc.Format.0 = match (desc.Format.0, d.Format.0) {
        (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
        (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
        (other, _) => other,
    };
    Some(desc)
}

/// Resolves the twin resource and host UAV description for a depth-texture UAV view.
pub(crate) fn view_resource<'a>(
    twin: &'a Twin,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<(&'a ID3D12Resource, D3D12_UNORDERED_ACCESS_VIEW_DESC)> {
    uav_for(&twin.colour.texture, desc)
        .map(|v| (&twin.colour.texture, v))
        .or_else(|| {
            twin.plane
                .as_ref()
                .and_then(|p| uav_for(&p.texture, desc).map(|v| (&p.texture, v)))
        })
}

/// Records copies between `depth` (`COPY_SOURCE`/`COPY_DEST`) and `twin.colour`.
/// Single-plane depth twins copy directly via `CopyResource`; depth-stencil
/// stencil planes copy Plane 1 through `twin.through` via `CopyTextureRegion`.
pub(crate) fn record_copy(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    twin: &Twin,
    to_twin: bool,
) {
    let (from, to) = if to_twin {
        (depth, &twin.colour.texture)
    } else {
        (&twin.colour.texture, depth)
    };
    let Some(through) = &twin.through else {
        // SAFETY: open list and matching copy-state resources.
        unsafe { list.CopyResource(to, from) };
        return;
    };
    let Some(host) = host() else { return };
    // SAFETY: live texture.
    let of = unsafe { depth.GetDesc() };
    let planes = u32::from(of.MipLevels).max(1) * u32::from(of.DepthOrArraySize).max(1);
    let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
    // SAFETY: valid query writing `planes` footprints for Plane 1 (stencil).
    unsafe {
        host.device
            .GetCopyableFootprints(&of, planes, planes, 0, Some(rows.as_mut_ptr()), None, None, None);
    }
    if rows.is_empty() || rows[0].Footprint.Format == DXGI_FORMAT_UNKNOWN {
        return;
    }
    let (first_from, first_to) = if to_twin { (planes, 0) } else { (0, planes) };
    let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(through) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    // SAFETY: open list, live resources; `through` rests in `COMMON` between copies.
    unsafe {
        list.ResourceBarrier(&[crate::host::transition(
            through,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
        for (i, row) in rows.iter().enumerate() {
            list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, first_from + i as u32), None);
        }
        list.ResourceBarrier(&[crate::host::transition(
            through,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        for (i, row) in rows.iter().enumerate() {
            list.CopyTextureRegion(&subresource(to, first_to + i as u32), 0, 0, 0, &placed(row), None);
        }
        list.ResourceBarrier(&[crate::host::transition(
            through,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    }
}

const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: std::sync::Mutex<Vec<(bool, i32, Option<(ID3D12RootSignature, ID3D12PipelineState)>)>>,
}
// SAFETY: D3D12 heaps, root signatures, and PSOs are thread-safe.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

fn compile(source: &str, entry: PCSTR, target: PCSTR) -> windows::core::Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: static strings and valid out pointers.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("twin"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let (Err(_), Some(errors)) = (&compiled, &errors) {
        // SAFETY: live blob bytes.
        let text = unsafe {
            std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
        };
        tracing::warn!(text = %String::from_utf8_lossy(text), "depth plane pass shader not compiled");
    }
    compiled?;
    let blob = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: live blob bytes.
    Ok(unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
    .to_vec())
}

impl PlanePass {
    fn new() -> windows::core::Result<Self> {
        let host = host().ok_or_else(windows::core::Error::empty)?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain shader-visible heap creation and start queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: PLANE_VIEWS,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            let cpu = views.GetCPUDescriptorHandleForHeapStart().ptr;
            let gpu = views.GetGPUDescriptorHandleForHeapStart().ptr;
            let step = host.device.GetDescriptorHandleIncrementSize(kind);
            Ok(Self {
                views,
                cpu,
                gpu,
                step,
                next: AtomicU32::new(0),
                made: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    fn pipeline(
        &self,
        steps: bool,
        format: DXGI_FORMAT,
    ) -> Option<(ID3D12RootSignature, ID3D12PipelineState)> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, p)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return p.clone();
        }
        let built = Self::make_pipeline(steps, format)
            .inspect_err(|e| tracing::warn!(error = %e, steps, format = format.0, "depth plane pass PSO refused"))
            .ok();
        made.push((steps, format.0, built.clone()));
        built
    }

    fn make_pipeline(
        steps: bool,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<(ID3D12RootSignature, ID3D12PipelineState)> {
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let ps = compile(if steps { PLANE_STEPS } else { PLANE_FLOATS }, s!("plane"), s!("ps_5_1"))?;
        let code = |b: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.as_ptr().cast(),
            BytecodeLength: b.len(),
        };
        // SAFETY: root signature is embedded in `ps`; descriptors point to live slices.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps)?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs),
                PS: code(&ps),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        PlanePass::new()
            .inspect_err(|e| tracing::warn!(error = %e, "depth plane pass unavailable"))
            .ok()
    })
    .as_ref()
}

/// Draws every slice and mip of depth-plane twin `plane` (`PIXEL_SHADER_RESOURCE`)
/// into `depth`'s depth plane (`DEPTH_WRITE`). Sets temporary pipeline, root
/// signature, heaps, targets, viewport, scissor, and topology on `list`.
pub(crate) fn record_plane_pass(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    let steps = twin.Format.0 == 53;
    let format = DXGI_FORMAT(match of.Format.0 {
        44 => 45,
        19 => 20,
        typed => typed,
    });
    let Some((root, pipeline)) = pass.pipeline(steps, format) else {
        return;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
                ..Default::default()
            };
            let dsv = host.dsv(depth, Some(&target));
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.cpu + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.gpu + u64::from(slot * pass.step),
            };
            // SAFETY: open list, `'static` pass objects, and valid views.
            unsafe {
                host.device.CreateShaderResourceView(plane, Some(&view), cpu);
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetGraphicsRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetGraphicsRootDescriptorTable(0, gpu);
                list.OMSetRenderTargets(0, None, false, Some(&dsv));
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                list.DrawInstanced(3, 1, 0, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Iid, S_OK, This, guid, slot};

    const RESOURCE_IID: Iid = guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    );

    #[test]
    fn wanted_accepts_single_plane_and_depth_stencil_formats() {
        const DEPTH_UAV: u32 = 6;
        assert!(wanted(53, DEPTH_UAV));
        assert!(wanted(55, DEPTH_UAV));
        assert!(wanted(39, DEPTH_UAV));
        assert!(wanted(40, DEPTH_UAV));
        assert!(wanted(118, DEPTH_UAV));
        assert!(wanted(119, DEPTH_UAV));
        assert!(wanted(120, DEPTH_UAV));
        assert!(wanted(44, DEPTH_UAV));
        assert!(wanted(19, DEPTH_UAV));
        assert!(!wanted(53, 2)); // no UNORDERED flag
        assert!(!wanted(28, DEPTH_UAV)); // colour format
    }

    fn upload_staging(host: &crate::host::Host, bytes: u64, init: &[u8]) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes.max(init.len() as u64),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buf: Option<ID3D12Resource> = None;
        // SAFETY: plain upload buffer creation + map/write.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut buf)
                .unwrap();
            let buf = buf.unwrap();
            let mut ptr = std::ptr::null_mut();
            buf.Map(0, None, Some(&mut ptr)).unwrap();
            std::ptr::copy_nonoverlapping(init.as_ptr(), ptr.cast::<u8>(), init.len());
            buf.Unmap(0, None);
            buf
        }
    }

    fn readback_staging(host: &crate::host::Host, bytes: u64) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buf: Option<ID3D12Resource> = None;
        // SAFETY: plain readback buffer creation.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buf)
                .unwrap();
        }
        buf.unwrap()
    }

    #[test]
    fn stencil_buffer_copy_and_array_depth_twins_work_without_stencil_ref() {
        let Some(host) = host() else { return };
        let _ = host.log_messages();

        // 1) Console D16_S8 (118) -> both stencil (R8_TYPELESS + through buffer)
        // and depth-plane (R16_TYPELESS) twins work even when PSSpecifiedStencilRefSupported=false.
        let ds_words: [u32; 14] = [3, 0, 0, 0, 4, 0, 4, 0x0001_0001, 118, 1, 0, 0, 6, 0];
        let mut ds_res = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                std::ptr::null(),
                0x10000,
                ds_words.as_ptr().cast(),
                0,
                std::ptr::null(),
                &RESOURCE_IID,
                &mut ds_res,
            ),
            S_OK
        );
        let twin = crate::resource::depth_twin(ds_res).expect("D16_S8 twin");
        assert!(twin.through.is_some());
        assert!(twin.plane.is_some());

        let stencil_uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_X24_TYPELESS_G8_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_UAV { MipSlice: 0, PlaneSlice: 1 },
            },
        };
        let (stex, sdesc) = view_resource(&twin, Some(&stencil_uav)).expect("stencil view");
        assert_eq!(sdesc.Format, DXGI_FORMAT_R8_UINT);
        assert_eq!(unsafe { stex.GetDesc() }.Format.0 as u32, R8_TYPELESS);

        let depth_uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R16_UNORM,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_UAV { MipSlice: 0, PlaneSlice: 0 },
            },
        };
        let (dtex, ddesc) = view_resource(&twin, Some(&depth_uav)).expect("depth-plane view");
        assert_eq!(ddesc.Format, DXGI_FORMAT_R16_UNORM);
        assert_eq!(unsafe { dtex.GetDesc() }.Format.0 as u32, 53);

        // Seed twin.colour (stencil twin) via upload buffer, sync via twin_use + resolve/close,
        // then read back Plane 1 (subresource 1) of the D24S8 depth texture and verify 0x5A!
        let list = host.new_list().unwrap();
        assert!(list.twin_use(ds_res, true, true));
        let mut st_fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut st_bytes = 0u64;
        // SAFETY: live resource query.
        unsafe {
            host.device.GetCopyableFootprints(
                &stex.GetDesc(),
                0,
                1,
                0,
                Some(&mut st_fp),
                None,
                None,
                Some(&mut st_bytes),
            );
        }
        let host_ds = crate::resource::host_resource(ds_res).unwrap();
        let mut dst_fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut dst_bytes = 0u64;
        // SAFETY: live resource query for Plane 1 (subresource 1).
        unsafe {
            host.device.GetCopyableFootprints(
                &host_ds.GetDesc(),
                1,
                1,
                0,
                Some(&mut dst_fp),
                None,
                None,
                Some(&mut dst_bytes),
            );
        }
        let pitch = st_fp.Footprint.RowPitch as usize;
        let mut init = vec![0u8; st_bytes as usize];
        for y in 0..4 {
            init[y * pitch..y * pitch + 4].fill(0x5A);
        }
        let up = upload_staging(host, st_bytes, &init);
        let rb = readback_staging(host, dst_bytes);
        // SAFETY: valid copy commands on open list.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                stex,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(stex),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&up),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: st_fp },
            };
            list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            list.list.ResourceBarrier(&[crate::host::transition(
                stex,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
        list.twins_written();
        assert!(list.twin_use(ds_res, false, false));
        crate::list::ensure_state(&list, ds_res, 0x800); // COPY_SOURCE
        // SAFETY: Plane 1 = subresource 1 of single-mip 4x4 D24S8.
        unsafe {
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&rb),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: dst_fp },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(host_ds),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 1 },
            };
            list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: GPU finished; plain readback map.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            rb.Map(0, None, Some(&mut ptr)).unwrap();
            let got = std::slice::from_raw_parts(ptr.cast::<u8>(), dst_bytes as usize);
            let dst_pitch = dst_fp.Footprint.RowPitch as usize;
            for y in 0..4 {
                assert_eq!(
                    &got[y * dst_pitch..y * dst_pitch + 4],
                    &[0x5A; 4],
                    "stencil twin must reach D24S8 plane 1 row {y}"
                );
            }
            rb.Unmap(0, None);
        }

        // 2) Format 53 (R16_TYPELESS shadow array, 3 slices) twin + UAV view works cleanly.
        let arr_words: [u32; 14] = [3, 0, 0, 0, 4, 0, 4, 0x0001_0003, 53, 1, 0, 0, 6, 0];
        let mut arr_res = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                std::ptr::null(),
                0x20000,
                arr_words.as_ptr().cast(),
                0,
                std::ptr::null(),
                &RESOURCE_IID,
                &mut arr_res,
            ),
            S_OK
        );
        let arr_twin = crate::resource::depth_twin(arr_res).expect("format 53 array twin");
        assert!(arr_twin.through.is_none());
        assert!(arr_twin.plane.is_none());
        let (atex, adesc) = view_resource(&arr_twin, None).expect("default array UAV");
        assert_eq!(adesc.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        assert_eq!(adesc.Format, DXGI_FORMAT_R16_UNORM);
        // SAFETY: live resource query.
        assert_eq!(unsafe { atex.GetDesc() }.DepthOrArraySize, 3);

        let list2 = host.new_list().unwrap();
        assert!(list2.twin_use(arr_res, true, true));
        list2.twins_written();
        list2.close();
        host.execute(&[&list2]);

        let msgs = host.log_messages();
        assert!(msgs.is_none(), "unexpected D3D12 validation errors: {msgs:?}");
        // SAFETY: plain removal-reason query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok(), "{msgs:?}");
        slot::<extern "system" fn(This) -> u32>(ds_res, 0x10)(ds_res);
        slot::<extern "system" fn(This) -> u32>(arr_res, 0x10)(arr_res);
    }
}
