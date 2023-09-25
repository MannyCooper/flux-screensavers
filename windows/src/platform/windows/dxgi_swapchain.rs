use std::borrow::Cow;
use std::ffi::CStr;
use std::fmt;
use std::mem;
use std::os::raw::{c_char, c_int, c_uint, c_void};

use glow as GL;
use glow::HasContext;
use raw_window_handle::RawWindowHandle;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11Texture2D, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::OpenGL::{wglGetCurrentDC, wglGetProcAddress};

#[derive(Debug)]
pub(crate) enum Problem {
    Unsupported,
    Failure(String),
}

impl From<&str> for Problem {
    fn from(s: &str) -> Self {
        Problem::Failure(s.to_owned())
    }
}
impl From<String> for Problem {
    fn from(s: String) -> Self {
        Problem::Failure(s)
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Unsupported => write!(f, "Unsupported"),
            Problem::Failure(s) => write!(f, "{}", s),
        }
    }
}

#[allow(dead_code)]
pub(crate) struct DXGIInterop {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain,
    gl_handle_d3d: HANDLE,
    dx_interop: WGLDXInteropExtensionFunctions,
    color_handle_gl: HANDLE,
    fbo: GL::NativeFramebuffer,
}

type GLint = c_int;
type GLenum = c_uint;
type GLuint = c_uint;

// const WGL_ACCESS_READ_WRITE_NV: u32 = 0x0001;
const WGL_ACCESS_READ_WRITE_DISCARD_NV: u32 = 0x0002;

#[allow(non_snake_case, dead_code)]
pub(crate) struct WGLDXInteropExtensionFunctions {
    pub(crate) DXCloseDeviceNV: unsafe extern "C" fn(hDevice: HANDLE) -> BOOL,
    pub(crate) DXLockObjectsNV:
        unsafe extern "C" fn(hDevice: HANDLE, count: GLint, hObjects: *mut HANDLE) -> BOOL,
    pub(crate) DXOpenDeviceNV: unsafe extern "C" fn(dxDevice: *mut c_void) -> HANDLE,
    pub(crate) DXRegisterObjectNV: unsafe extern "C" fn(
        hDevice: HANDLE,
        dxResource: *mut c_void,
        name: GLuint,
        object_type: GLenum,
        access: GLenum,
    ) -> HANDLE,
    pub(crate) DXSetResourceShareHandleNV:
        unsafe extern "C" fn(dxResource: *mut c_void, shareHandle: HANDLE) -> BOOL,
    pub(crate) DXUnlockObjectsNV:
        unsafe extern "C" fn(hDevice: HANDLE, count: GLint, hObjects: *mut HANDLE) -> BOOL,
    pub(crate) DXUnregisterObjectNV: unsafe extern "C" fn(hDevice: HANDLE, hObject: HANDLE) -> BOOL,
}

pub(crate) unsafe fn with_dxgi_swapchain(
    dxgi_interop: &mut DXGIInterop,
    render: impl FnOnce(&GL::NativeFramebuffer),
) {
    (dxgi_interop.dx_interop.DXLockObjectsNV)(
        dxgi_interop.gl_handle_d3d,
        1,
        &mut dxgi_interop.color_handle_gl as *mut _,
    );

    render(&dxgi_interop.fbo);

    (dxgi_interop.dx_interop.DXUnlockObjectsNV)(
        dxgi_interop.gl_handle_d3d,
        1,
        &mut dxgi_interop.color_handle_gl as *mut _,
    );

    let _ = dxgi_interop.swap_chain.Present(1, 0);
}

// Detect Intel GPUs.
// The Intel drivers don't play well with the DXGI interop extension.
pub(crate) fn is_intel_gpu(gl: &glow::Context) -> bool {
    let vendor = unsafe { gl.get_parameter_string(GL::VENDOR) };
    log::debug!("OpenGL Vendor: {}", vendor);
    vendor.contains("Intel")
}

// https://github.com/Osspial/render_to_dxgi/blob/master/src/main.rs
// https://github.com/nlguillemot/OpenGL-on-DXGI/blob/master/main.cpp
#[allow(non_snake_case)]
pub(crate) fn create_dxgi_swapchain(
    raw_window_handle: &RawWindowHandle,
    gl: &glow::Context,
) -> Result<DXGIInterop, Problem> {
    if is_intel_gpu(gl) {
        log::debug!("Intel GPU detected. Disabling DXGI swapchain");
        return Err(Problem::Unsupported);
    }

    let win32_handle = match raw_window_handle {
        RawWindowHandle::Win32(handle) => handle,
        _ => return Err("Only Win32 handles can be used to create a DXGI swapchain".into()),
    };

    let hwnd = HWND(win32_handle.hwnd as _);

    let mut p_device: Option<ID3D11Device> = None;
    let mut p_context: Option<ID3D11DeviceContext> = None;
    let mut p_swap_chain: Option<IDXGISwapChain> = None;

    unsafe {
        D3D11CreateDeviceAndSwapChain(
            None,                        // Adapter
            D3D_DRIVER_TYPE_HARDWARE,    // Driver type
            None,                        // Software
            D3D11_CREATE_DEVICE_FLAG(0), // Flags (do not set D3D11_CREATE_DEVICE_SINGLETHREADED)
            None,                        // Feature levels
            D3D11_SDK_VERSION,           // SDK version
            Some(&DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    ..Default::default()
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                OutputWindow: hwnd,
                Windowed: true.into(),
                // FLIP modes don't work on NVIDIA cards.
                SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
                SampleDesc: DXGI_SAMPLE_DESC {
                    // Disable MSAA (also unsupported with the 'flip' model)
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            }),
            Some(&mut p_swap_chain),
            Some(&mut p_device),
            None,
            Some(&mut p_context),
        )
        .map_err(|_| "Failed to create DXGI device and swapchain")?;
    }

    let swap_chain = p_swap_chain.expect("failed to created swapchain");
    let context = p_context.expect("failed to create immediate context");
    let device = p_device.expect("failed to create device");

    log::debug!("Created device, context, and swapchain");

    log::debug!("Fetching WGL extensions");

    unsafe {
        let dc = wglGetCurrentDC();
        let get_extensions_string_arb: Option<unsafe extern "C" fn(hdc: HDC) -> *const c_char> =
            mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglGetExtensionsStringARB\0"[0] as *const u8,
            )));

        let extensions = match get_extensions_string_arb {
            Some(wglGetExtensionsStringARB) => {
                CStr::from_ptr(wglGetExtensionsStringARB(dc)).to_string_lossy()
            }
            None => Cow::Borrowed(""),
        };

        log::debug!("Supported extensions: {}", extensions);

        // Check if WGL_NV_DX_interop2 is supported
        if !extensions.contains("WGL_NV_DX_interop2") {
            return Err(Problem::Unsupported);
        }
    }

    let dx_interop = unsafe {
        WGLDXInteropExtensionFunctions {
            DXCloseDeviceNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXCloseDeviceNV\0"[0] as *const u8,
            ))),
            DXLockObjectsNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXLockObjectsNV\0"[0] as *const u8,
            ))),
            DXOpenDeviceNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXOpenDeviceNV\0"[0] as *const u8,
            ))),
            DXRegisterObjectNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXRegisterObjectNV\0"[0] as *const u8,
            ))),
            DXSetResourceShareHandleNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXSetResourceShareHandleNV\0"[0] as *const u8,
            ))),
            DXUnlockObjectsNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXUnlockObjectsNV\0"[0] as *const u8,
            ))),
            DXUnregisterObjectNV: mem::transmute(wglGetProcAddress(PCSTR(
                &b"wglDXUnregisterObjectNV\0"[0] as *const u8,
            ))),
        }
    };
    log::debug!("Fetched interop extension functions");

    unsafe {
        // Fetch the swapchain buffer
        let color_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0).unwrap();
        let mut color_buffer_view: Option<ID3D11RenderTargetView> = None;

        // Create view
        device
            .CreateRenderTargetView(&color_buffer, None, Some(&mut color_buffer_view))
            .unwrap();

        // Attach the back buffer to the render target for the device
        context.OMSetRenderTargets(Some(&[color_buffer_view.clone()]), None);

        // Clear the back buffer
        let clear_color: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
        context.ClearRenderTargetView(
            color_buffer_view.as_ref().unwrap(),
            &clear_color as *const _,
        );
        log::debug!("Cleared render target view");

        // Register the D3D11 device with GL
        let gl_handle_d3d = (dx_interop.DXOpenDeviceNV)(device.as_raw());
        if gl_handle_d3d.is_invalid() {
            let msg = std::io::Error::last_os_error();
            return Err(format!(
                "Failed to open the GL DX interop device. OS Error: {:?}",
                msg
            )
            .into());
        }

        log::debug!("Opened GL DX interop device");

        let fbo = gl.create_framebuffer().unwrap();
        let rbo = gl.create_renderbuffer().unwrap();

        let mut color_handle_gl = (dx_interop.DXRegisterObjectNV)(
            gl_handle_d3d,
            color_buffer.as_raw(),
            rbo.0.into(),
            GL::RENDERBUFFER,
            WGL_ACCESS_READ_WRITE_DISCARD_NV,
        );

        if color_handle_gl.is_invalid() {
            log::warn!("Failed to register a renderbuffer with DXGI. Falling back to a texture.");

            gl.delete_renderbuffer(rbo);
            let texture = gl.create_texture().unwrap();

            // According to my testing, AMD graphics cards don't support sharing renderbuffers.
            color_handle_gl = (dx_interop.DXRegisterObjectNV)(
                gl_handle_d3d,
                color_buffer.as_raw(),
                texture.0.into(),
                GL::TEXTURE_2D,
                WGL_ACCESS_READ_WRITE_DISCARD_NV,
            );

            if color_handle_gl.is_invalid() {
                let msg = std::io::Error::last_os_error();
                return Err(
                    format!("Failed to register texture with DXGI. OS Error: {:?}", msg).into(),
                );
            }

            log::debug!("Registered DXGI swapchain as GL texture");

            // Bind the texture to the framebuffer
            gl.bind_framebuffer(GL::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                GL::FRAMEBUFFER,
                GL::COLOR_ATTACHMENT0,
                GL::TEXTURE_2D,
                Some(texture),
                0,
            );
        } else {
            log::debug!("Registered DXGI swapchain as GL renderbuffer");

            gl.bind_framebuffer(GL::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_renderbuffer(
                GL::FRAMEBUFFER,
                GL::COLOR_ATTACHMENT0,
                GL::RENDERBUFFER,
                Some(rbo),
            );
        }

        match gl.check_framebuffer_status(GL::FRAMEBUFFER) {
            GL::FRAMEBUFFER_COMPLETE => {
                log::debug!("GL Framebuffer complete");
            }
            // Nvidia complains that the buffer is unsupported, but it still works. *shrug*
            GL::FRAMEBUFFER_UNSUPPORTED => log::debug!("GL Framebuffer unsupported"),
            GL::FRAMEBUFFER_INCOMPLETE_ATTACHMENT => {
                return Err("GL Framebuffer incomplete attachment".into())
            }
            GL::FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT => {
                return Err("GL Framebuffer missing attachment".into())
            }
            other => return Err(format!("DXGI Framebuffer: {:#x}", other).into()),
        }

        gl.bind_framebuffer(GL::FRAMEBUFFER, None);

        Ok(DXGIInterop {
            device,
            context,
            swap_chain,
            gl_handle_d3d,
            dx_interop,
            color_handle_gl,
            fbo,
        })
    }
}
