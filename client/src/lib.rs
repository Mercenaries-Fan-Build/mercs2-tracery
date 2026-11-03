//! mercs2_profiler.asi: Tracy zones and frame marks from the running Mercenaries 2, on m2-sdk.dll's hooks,
//! with an in-game overlay and a hotkey that starts and stops capture sessions. Everything but [`win`] and
//! [`overlay`] is plain logic, tested on the host.

pub mod atlas;
pub mod cells;
pub mod config;
#[cfg(all(windows, target_arch = "x86"))]
pub mod crash;
pub mod shadow;
pub mod cpu;
pub mod family_drop;
pub mod jobs;
pub mod live;
pub mod route;
pub mod stores;
pub mod samples_stream;
pub mod session;

#[cfg(all(windows, target_arch = "x86"))]
mod overlay;
#[cfg(all(windows, target_arch = "x86"))]
mod pool;
#[cfg(all(windows, target_arch = "x86"))]
mod sampler;
#[cfg(all(windows, target_arch = "x86"))]
mod threads;
#[cfg(all(windows, target_arch = "x86"))]
mod win;
