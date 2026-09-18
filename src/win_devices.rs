//! Windows-only audio-endpoint enumeration.
//!
//! cpal's WASAPI backend only lists endpoints in the `ACTIVE` device state
//! (it calls `EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)`), so mics that
//! Windows has disabled, unplugged or left unconfigured (e.g. a headset mic
//! on a combo jack, or a Bluetooth headset in A2DP-only mode) are invisible
//! to the normal device list. Windows itself still knows about them.
//!
//! These helpers enumerate *every* endpoint instead, so the UI can show the
//! full picture and (once an endpoint is enabled in Windows) capture from it.

#![allow(clippy::missing_safety_doc)]

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eRender, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT,
    DEVICE_STATE_UNPLUGGED, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};

const VT_LPSTR: u16 = 30;
const VT_LPWSTR: u16 = 31;

/// Returns the friendly names of all audio endpoints for the given data flow,
/// regardless of their Windows device state.
pub fn enumerate_all_audio_endpoints(capture: bool) -> Vec<String> {
    use windows::Win32::Media::Audio::DEVICE_STATE;

    unsafe {
        // COM must be initialized for CoCreateInstance. MTA is right for a
        // non-interactive audio app; RPC_E_CHANGED_MODE is fine to ignore.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let flow: EDataFlow = if capture { eCapture } else { eRender };
        let state_mask = DEVICE_STATE(
            DEVICE_STATE_ACTIVE.0
                | DEVICE_STATE_DISABLED.0
                | DEVICE_STATE_NOTPRESENT.0
                | DEVICE_STATE_UNPLUGGED.0,
        );
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let Ok(collection) = enumerator.EnumAudioEndpoints(flow, state_mask) else {
            return Vec::new();
        };
        let Ok(count) = collection.GetCount() else {
            return Vec::new();
        };

        let mut names = Vec::new();
        for i in 0..count {
            let Ok(device) = collection.Item(i) else { continue };
            let Ok(store) = device.OpenPropertyStore(STGM_READ) else { continue };
            let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) else { continue };
            let raw = prop.as_raw();
            let vt = raw.Anonymous.Anonymous.vt;
            let name = if vt == VT_LPWSTR {
                let wide = windows::core::PWSTR(raw.Anonymous.Anonymous.Anonymous.pwszVal);
                wide.to_string().ok()
            } else if vt == VT_LPSTR {
                let narrow = windows::core::PSTR(raw.Anonymous.Anonymous.Anonymous.pszVal);
                narrow.to_string().ok()
            } else {
                None
            };
            if let Some(name) = name {
                if !name.trim().is_empty() {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
}