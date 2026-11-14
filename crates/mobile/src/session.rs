//! A server signed in to with a token, as this device.

use crate::folder::Folder;
use crate::{bad, Result};
use cairn_engine::client::{Client, Retries};
use cairn_engine::root::Config;
use cairn_protocol::wire::Access;
use cairn_protocol::DeviceId;
use std::sync::Arc;

/// A folder the token reaches.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FolderInfo {
    /// The namespace, as the server names it.
    pub folder: String,
    /// The user's own name for it, shown in its place.
    pub name: Option<String>,
    /// `read`, `write` or `manage`.
    pub access: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Me {
    pub user: String,
    pub folders: Vec<FolderInfo>,
}

pub(crate) fn access_text(a: Access) -> String {
    match a {
        Access::Read => "read",
        Access::Write => "write",
        Access::Manage => "manage",
    }
    .to_string()
}

#[derive(uniffi::Object)]
pub struct Session {
    pub(crate) server: String,
    pub(crate) token: String,
    pub(crate) device: DeviceId,
    pub(crate) device_name: String,
    pub(crate) allow_http: bool,
    client: Client,
}

#[uniffi::export]
impl Session {
    /// A session with a server: no request is made until one is asked for. `device` is the ID
    /// `new_device_id` made when the app was first signed in.
    #[uniffi::constructor]
    pub fn new(
        server: String,
        token: String,
        device: String,
        device_name: String,
        allow_http: bool,
    ) -> Result<Arc<Self>> {
        let server = server.trim().trim_end_matches('/').to_string();
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(bad("the token is empty"));
        }
        let device_name = device_name.trim().to_string();
        if device_name.is_empty() {
            return Err(bad("the device needs a name"));
        }
        let device: DeviceId = device
            .parse()
            .map_err(|_| bad(format!("{device:?} is not a device ID")))?;
        let client = Client::of_server(&server, &token, allow_http)?;
        Ok(Arc::new(Self {
            server,
            token,
            device,
            device_name,
            allow_http,
            client,
        }))
    }

    /// Who the token is, and every folder it reaches: the check a sign-in makes.
    pub fn me(&self) -> Result<Me> {
        let me = self.client.me()?;
        Ok(Me {
            user: me.user,
            folders: me
                .folders
                .into_iter()
                .map(|f| FolderInfo {
                    folder: f.folder,
                    name: f.name,
                    access: access_text(f.access),
                })
                .collect(),
        })
    }

    /// One folder, asked for its `/info` first: the protocol it speaks and this token's access there.
    pub fn folder(&self, folder: String) -> Result<Arc<Folder>> {
        let config = Config {
            server: self.server.clone(),
            namespace: folder.clone(),
            token: self.token.clone(),
            device: self.device,
            device_name: self.device_name.clone(),
            policy: Default::default(),
            filenames: Default::default(),
            allow_http: self.allow_http,
        };
        let client = Client::new(&config, Retries::Short)?;
        Ok(Arc::new(Folder::new(
            folder,
            client,
            self.device,
            self.device_name.clone(),
        )))
    }

    /// The `cairn` cookie's value for a web session as this token, which a video player sends.
    pub fn session_cookie(&self) -> Result<String> {
        Ok(self.client.session_cookie()?)
    }

    pub fn server(&self) -> String {
        self.server.clone()
    }

    pub fn device_name(&self) -> String {
        self.device_name.clone()
    }
}
