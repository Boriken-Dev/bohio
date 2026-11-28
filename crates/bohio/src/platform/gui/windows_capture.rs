//! Screen capture on Windows: DXGI Desktop Duplication (one duplication per display, kept on a thread of its own while
//! captures keep coming) and GDI (`BitBlt` from the screen, for what duplication cannot do). Pixels leave as RGB, rows
//! from the top; no pointer is drawn into them (the pointer is reported apart).
//!
//! This module is one of the files that hold `unsafe`.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::core::Interface;

use bohio_proto::gui::Rect;

use bohio_x11::{Frame, GuiError};

/// A display to capture: the name Windows gives its device, and where it lies.
#[derive(Debug, Clone)]
pub(super) struct Target {
    /// `\\.\DISPLAY1`, the name the output of the duplication and the monitor share.
    pub device: String,
    /// The display's top-left corner on the virtual screen.
    pub origin: (i32, i32),
    /// Whether the display is rotated, which duplication does not undo.
    pub rotated: bool,
}

/// What one capture asks of the thread that holds the duplications.
struct Job {
    target: Target,
    region: Rect,
    reply: SyncSender<Result<Frame, GuiError>>,
}

/// The settings the duplication thread runs by.
#[derive(Debug, Clone, Copy)]
pub(super) struct Timing {
    /// How long to wait for a display's first frame.
    pub frame_wait: Duration,
    /// How long the duplications are kept after the last capture.
    pub idle: Duration,
}

struct Worker {
    sender: Sender<Job>,
    thread: Option<JoinHandle<()>>,
}

/// DXGI capture: a thread that holds the duplications, started by the first capture and ended with this value.
pub(super) struct Duplication {
    timing: Timing,
    worker: OnceLock<Result<Worker, String>>,
}

impl Duplication {
    pub(super) const fn new(timing: Timing) -> Self {
        Self {
            timing,
            worker: OnceLock::new(),
        }
    }

    /// `region` of the display `target` through DXGI.
    ///
    /// # Errors
    ///
    /// `unavailable` when duplication cannot serve the display (a rotated one, a session without a GPU output, access
    /// lost to the secure desktop) and `io` for a failure of the capture itself.
    pub(super) fn grab(&self, target: &Target, region: Rect) -> Result<Frame, GuiError> {
        if target.rotated {
            return Err(GuiError::unavailable(
                "DXGI duplication does not undo a display's rotation",
            ));
        }
        let timing = self.timing;
        let worker = self
            .worker
            .get_or_init(|| {
                let (sender, receiver) = mpsc::channel::<Job>();
                thread::Builder::new()
                    .name("bohio-gui-dxgi".to_owned())
                    .spawn(move || serve(&receiver, timing))
                    .map(|thread| Worker {
                        sender,
                        thread: Some(thread),
                    })
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(|error| GuiError::io(format!("the capture thread did not start: {error}")))?;
        let (reply, answer) = mpsc::sync_channel(1);
        worker
            .sender
            .send(Job {
                target: target.clone(),
                region,
                reply,
            })
            .map_err(|_| GuiError::io("the capture thread ended"))?;
        answer
            .recv()
            .map_err(|_| GuiError::io("the capture thread ended"))?
    }
}

impl Drop for Duplication {
    fn drop(&mut self) {
        if let Some(Ok(mut worker)) = self.worker.take() {
            let Worker { sender, thread } = &mut worker;
            // The thread ends when its channel does.
            drop(std::mem::replace(sender, mpsc::channel().0));
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

fn serve(receiver: &mpsc::Receiver<Job>, timing: Timing) {
    let mut held: HashMap<String, Output> = HashMap::new();
    loop {
        let job = if held.is_empty() {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(timing.idle)
        };
        match job {
            Ok(job) => {
                let answer = capture(&mut held, &job, timing.frame_wait);
                if answer.is_err() {
                    // A duplication that failed is rebuilt by the next capture.
                    held.remove(&job.target.device);
                }
                if timing.idle.is_zero() {
                    held.clear();
                }
                let _ = job.reply.send(answer);
            }
            Err(RecvTimeoutError::Timeout) => held.clear(),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// One display's duplication, the copy of its latest frame and the buffer the pixels are read through.
struct Output {
    duplication: IDXGIOutputDuplication,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    /// The latest frame the duplication delivered, kept because it delivers only what changed.
    shadow: ID3D11Texture2D,
    staging: Option<((u32, u32), ID3D11Texture2D)>,
    size: (u32, u32),
    have_frame: bool,
}

fn io(what: &str, error: &windows::core::Error) -> GuiError {
    GuiError::io(format!("{what}: {error}"))
}

fn capture(
    held: &mut HashMap<String, Output>,
    job: &Job,
    frame_wait: Duration,
) -> Result<Frame, GuiError> {
    if !held.contains_key(&job.target.device) {
        let output = open(&job.target.device)?;
        held.insert(job.target.device.clone(), output);
    }
    let output = held
        .get_mut(&job.target.device)
        .ok_or_else(|| GuiError::io("the duplication is gone"))?;
    output.refresh(frame_wait)?;
    output.read(job.region)
}

/// The duplication of the output whose device is `device`, on the adapter that drives it.
fn open(device: &str) -> Result<Output, GuiError> {
    // SAFETY: plain DXGI and Direct3D 11 calls. Every pointer passed is to a local that lives through the call, and every
    // interface returned is owned by the windows crate's smart pointer, which releases it.
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|error| io("DXGI is not available", &error))?;
        let mut adapter_index = 0;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(adapter_index) {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => {
                    return Err(GuiError::unavailable(
                        "no graphics adapter drives that display for duplication",
                    ));
                }
                Err(error) => return Err(io("listing the graphics adapters", &error)),
            };
            adapter_index += 1;
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                output_index += 1;
                let described = output
                    .GetDesc()
                    .map_err(|error| io("describing an output", &error))?;
                let name = wide_to_string(&described.DeviceName);
                if !name.eq_ignore_ascii_case(device) {
                    continue;
                }
                if described.Rotation != DXGI_MODE_ROTATION_IDENTITY {
                    return Err(GuiError::unavailable(
                        "DXGI duplication does not undo a display's rotation",
                    ));
                }
                return duplicate(
                    &adapter,
                    &output
                        .cast::<IDXGIOutput1>()
                        .map_err(|error| io("the output has no duplication interface", &error))?,
                );
            }
        }
    }
}

/// The duplication of `output`, with a Direct3D device made on `adapter`.
fn duplicate(adapter: &IDXGIAdapter1, output: &IDXGIOutput1) -> Result<Output, GuiError> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: the out pointers are to locals; the adapter is a live interface.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| io("creating the Direct3D device", &error))?;
    let (Some(device), Some(context)) = (device, context) else {
        return Err(GuiError::io("Direct3D returned no device"));
    };
    // SAFETY: `device` is the live device just created on the output's adapter.
    let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(|error| {
        // E_ACCESSDENIED while the secure desktop has the screen, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE past the system's
        // limit of duplications.
        GuiError::unavailable(format!("desktop duplication was refused: {error}"))
    })?;
    // SAFETY: a plain query of the duplication just made.
    let described = unsafe { duplication.GetDesc() };
    if described.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(GuiError::unavailable(
            "the display's format is not 8-bit BGRA (an HDR display); GDI captures it",
        ));
    }
    let size = (described.ModeDesc.Width, described.ModeDesc.Height);
    let shadow = texture(&device, size, D3D11_USAGE_DEFAULT, 0)?;
    Ok(Output {
        duplication,
        context,
        device,
        shadow,
        staging: None,
        size,
        have_frame: false,
    })
}

fn texture(
    device: &ID3D11Device,
    size: (u32, u32),
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    cpu: u32,
) -> Result<ID3D11Texture2D, GuiError> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: 0,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut made: Option<ID3D11Texture2D> = None;
    // SAFETY: the description and the out pointer are locals that live through the call.
    unsafe { device.CreateTexture2D(&description, None, Some(&mut made)) }
        .map_err(|error| io("creating a texture", &error))?;
    made.ok_or_else(|| GuiError::io("Direct3D returned no texture"))
}

impl Output {
    /// Takes the duplication's latest frame into the shadow copy, when there is one: it delivers a frame only when the
    /// display changed, so a still display is read from the copy. The first capture waits up to `frame_wait` for a frame.
    fn refresh(&mut self, frame_wait: Duration) -> Result<(), GuiError> {
        let deadline = Instant::now() + frame_wait;
        loop {
            let wait = if self.have_frame {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: the out pointers are to locals; the duplication is live. The frame is released below before the
            // next acquire, as the API requires.
            let acquired = unsafe {
                self.duplication.AcquireNextFrame(
                    u32::try_from(wait.as_millis()).unwrap_or(u32::MAX),
                    &mut info,
                    &mut resource,
                )
            };
            match acquired {
                Ok(()) => {
                    // A frame that moved only the pointer carries no new picture.
                    let picture = info.LastPresentTime != 0 || info.AccumulatedFrames > 0;
                    let copied = match (&resource, picture) {
                        (Some(resource), true) => resource
                            .cast::<ID3D11Texture2D>()
                            .map(|frame| {
                                // SAFETY: both textures belong to this device, are the same size and format, and the
                                // frame is held until the release below.
                                unsafe { self.context.CopyResource(&self.shadow, &frame) };
                            })
                            .map_err(|error| io("reading the frame", &error)),
                        _ => Ok(()),
                    };
                    // SAFETY: the frame was acquired just above.
                    let released = unsafe { self.duplication.ReleaseFrame() };
                    copied?;
                    released.map_err(|error| io("releasing the frame", &error))?;
                    if picture {
                        self.have_frame = true;
                        return Ok(());
                    }
                    if self.have_frame {
                        return Ok(());
                    }
                }
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    if self.have_frame {
                        return Ok(());
                    }
                    return Err(GuiError::unavailable(
                        "desktop duplication delivered no frame in time",
                    ));
                }
                Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                    return Err(GuiError::unavailable(
                        "desktop duplication was lost (a mode change, or the secure desktop took the screen)",
                    ));
                }
                Err(error) => return Err(io("acquiring a frame", &error)),
            }
            if Instant::now() >= deadline {
                return Err(GuiError::unavailable(
                    "desktop duplication delivered no picture in time",
                ));
            }
        }
    }

    /// `region` of the shadow copy as RGB.
    fn read(&mut self, region: Rect) -> Result<Frame, GuiError> {
        if region.x.saturating_add(region.width) > self.size.0
            || region.y.saturating_add(region.height) > self.size.1
        {
            return Err(GuiError::bad_request(
                "the region lies outside the display (it changed size)",
            ));
        }
        let wanted = (region.width, region.height);
        if self
            .staging
            .as_ref()
            .is_none_or(|(size, _)| *size != wanted)
        {
            let staging = texture(
                &self.device,
                wanted,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ.0 as u32,
            )?;
            self.staging = Some((wanted, staging));
        }
        let (_, staging) = self
            .staging
            .as_ref()
            .ok_or_else(|| GuiError::io("no staging texture"))?;
        let part = D3D11_BOX {
            left: region.x,
            top: region.y,
            front: 0,
            right: region.x + region.width,
            bottom: region.y + region.height,
            back: 1,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures belong to this device; the box lies inside the shadow (checked above) and the staging
        // texture is exactly its size. The mapping is read only while it is held, then unmapped, and `pitch` rows of
        // `width` pixels lie inside what the driver mapped.
        let rgb = unsafe {
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, &self.shadow, 0, Some(&part));
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|error| io("mapping the frame", &error))?;
            let pitch = mapped.RowPitch as usize;
            let rows = region.height as usize;
            let width = region.width as usize;
            // The last row is not padded out to the pitch in what the driver mapped.
            let length = pitch * (rows - 1) + width * 4;
            let bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), length);
            let rgb = bgra_to_rgb(bytes, pitch, width, rows);
            self.context.Unmap(staging, 0);
            rgb
        };
        Frame::new(region.width, region.height, rgb)
    }
}

/// `rows` rows of `width` BGRA pixels, `pitch` bytes apart, as tightly packed RGB.
fn bgra_to_rgb(bytes: &[u8], pitch: usize, width: usize, rows: usize) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(width * rows * 3);
    for row in bytes.chunks(pitch).take(rows) {
        for pixel in row[..width * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    rgb
}

fn wide_to_string(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// `region` of the display at `origin`, through GDI: a copy of what the screen shows, from the calling thread.
///
/// # Errors
///
/// `io` when the operating system refuses a call.
pub(super) fn gdi(origin: (i32, i32), region: Rect) -> Result<Frame, GuiError> {
    let width =
        i32::try_from(region.width).map_err(|_| GuiError::bad_request("the region is too wide"))?;
    let height = i32::try_from(region.height)
        .map_err(|_| GuiError::bad_request("the region is too tall"))?;
    let left = i32::try_from(region.x)
        .ok()
        .and_then(|x| origin.0.checked_add(x))
        .ok_or_else(|| GuiError::bad_request("the region lies off the screen"))?;
    let top = i32::try_from(region.y)
        .ok()
        .and_then(|y| origin.1.checked_add(y))
        .ok_or_else(|| GuiError::bad_request("the region lies off the screen"))?;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative: rows from the top.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    // SAFETY: GDI calls on handles made here and released before returning. `bits` points into the DIB section, which
    // holds `width * height * 4` bytes while the bitmap lives, and is read only before the bitmap is deleted.
    unsafe {
        let screen: HDC = GetDC(None);
        if screen.is_invalid() {
            return Err(GuiError::io("the screen's device context is not available"));
        }
        let memory = CreateCompatibleDC(Some(screen));
        let cleanup = |memory: HDC| {
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        };
        if memory.is_invalid() {
            cleanup(memory);
            return Err(GuiError::io("a memory device context was not made"));
        }
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) if !bits.is_null() => bitmap,
            Ok(bitmap) => {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                cleanup(memory);
                return Err(GuiError::io("the bitmap has no pixels"));
            }
            Err(error) => {
                cleanup(memory);
                return Err(io("making a bitmap", &error));
            }
        };
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            left,
            top,
            SRCCOPY | CAPTUREBLT,
        );
        let rgb = copied.as_ref().ok().map(|()| {
            let bytes = std::slice::from_raw_parts(
                bits.cast::<u8>(),
                region.width as usize * region.height as usize * 4,
            );
            bgra_to_rgb(
                bytes,
                region.width as usize * 4,
                region.width as usize,
                region.height as usize,
            )
        });
        SelectObject(memory, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        cleanup(memory);
        match (rgb, copied) {
            (Some(rgb), _) => Frame::new(region.width, region.height, rgb),
            (None, Err(error)) => Err(io("copying the screen", &error)),
            (None, Ok(())) => Err(GuiError::io("copying the screen failed")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bgra_to_rgb;

    #[test]
    fn rows_are_read_by_the_pitch_and_the_last_row_may_be_short() {
        // Two pixels a row, a pitch of 12 bytes (4 of padding), two rows; the last row is cut at its pixels.
        let bytes: Vec<u8> = vec![
            1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9, // b g r a, b g r a, padding
            7, 8, 9, 255, 10, 11, 12, 255,
        ];
        assert_eq!(
            bgra_to_rgb(&bytes, 12, 2, 2),
            [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]
        );
    }
}
