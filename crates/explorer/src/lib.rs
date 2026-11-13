//! Cairn's badges in Windows Explorer: four icon overlay handlers (`cairn_protocol::local::OVERLAYS`),
//! in one DLL that Explorer loads, and with it every program that shows a file dialog. Each says
//! whether an entry has its badge, as the daemon of the entry's root tells it on the local API
//! (`Request::Badges`, `Request::WatchBadges`): nothing is decided here, so Explorer shows what
//! `cairn status` would say.
//!
//! `cairn explorer on` puts the DLL and its icons in the user's Cairn folder and registers the handlers;
//! Explorer takes them up when it next starts. What is in no root, the great part of what Explorer shows,
//! costs a comparison with the roots' paths and nothing more.
//!
//! Explorer waits on each handler, so no request is made on its threads: a folder not known yet is
//! asked for on a thread of this DLL's, its entries show no badge until it is answered, and Explorer is
//! then told to look at them again. One more thread for each root shown watches its daemon for
//! changes, and asks again for the folders they touch.

pub mod cache;
#[cfg(windows)]
mod com;
#[cfg(windows)]
mod worker;

#[cfg(windows)]
pub use com::{DllCanUnloadNow, DllGetClassObject};
