//! Device enumeration + the shared audio format.

use anyhow::{bail, Result};
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

/// COM interfaces are not Send, so we pass plain IDs between threads and
/// let each thread re-open its own IMMDevice.
pub struct Endpoint {
    pub id: String,
    pub name: String,
}

pub fn enumerate_render() -> Result<Vec<Endpoint>> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let coll = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let dev = coll.Item(i)?;
            let id = dev.GetId()?.to_string()?;
            let name = friendly_name(&dev)?;
            out.push(Endpoint { id, name });
        }
        Ok(out)
    }
}

unsafe fn friendly_name(dev: &IMMDevice) -> Result<String> {
    let store = dev.OpenPropertyStore(STGM_READ)?;
    let v = store.GetValue(&PKEY_Device_FriendlyName)?;

    // PROPVARIANT layout: u16 vt, 6 bytes reserved, union at offset 8.
    // For VT_LPWSTR (31) the union holds a *const u16.
    let base = &v as *const _ as *const u8;
    let vt = std::ptr::read_unaligned(base as *const u16);
    if vt != 31 {
        return Ok(String::from("<unknown device>"));
    }
    let ptr = std::ptr::read_unaligned(base.add(8) as *const *const u16);
    Ok(PCWSTR(ptr).to_string()?)
    // `v` is dropped here and frees the string itself.
}

/// Call after CoInitializeEx on the current thread.
pub unsafe fn open_by_id(id: &str) -> Result<IMMDevice> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    Ok(en.GetDevice(PCWSTR(wide.as_ptr()))?)
}

/// Owned copy of a WAVEFORMATEX(EXTENSIBLE). u64 storage keeps it aligned.
#[derive(Clone)]
pub struct Fmt {
    buf: Vec<u64>,
    pub channels: usize,
    pub rate: u32,
}

impl Fmt {
    /// Reads the shared-mode mix format of a device (always f32 on modern Windows).
    pub unsafe fn from_mix(client: &IAudioClient) -> Result<Fmt> {
        let p = client.GetMixFormat()?;
        let size = std::mem::size_of::<WAVEFORMATEX>() + (*p).cbSize as usize;
        let mut buf = vec![0u64; (size + 7) / 8];
        std::ptr::copy_nonoverlapping(p as *const u8, buf.as_mut_ptr() as *mut u8, size);
        let channels = (*p).nChannels as usize;
        let rate = (*p).nSamplesPerSec;
        let bits = (*p).wBitsPerSample;
        CoTaskMemFree(Some(p as *const _));
        if bits != 32 {
            bail!("expected 32-bit float mix format, got {bits} bits");
        }
        Ok(Fmt {
            buf,
            channels,
            rate,
        })
    }

    pub fn ptr(&self) -> *const WAVEFORMATEX {
        self.buf.as_ptr() as *const WAVEFORMATEX
    }
}
