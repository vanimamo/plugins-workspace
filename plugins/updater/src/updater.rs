// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::Cursor,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use base64::Engine;
use futures_util::StreamExt;
use http::HeaderName;
use minisign_verify::{PublicKey, Signature};
use percent_encoding::{AsciiSet, CONTROLS};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    ClientBuilder, StatusCode,
};
use semver::Version;
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize};
use tauri::{utils::platform::current_exe, Resource};
use time::OffsetDateTime;
use url::Url;

use crate::{
    error::{Error, Result},
    Config,
};

const UPDATER_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ReleaseManifestPlatform {
    /// Download URL for the platform
    pub url: Url,
    /// Signature for the platform
    pub signature: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum RemoteReleaseInner {
    Dynamic(ReleaseManifestPlatform),
    Static {
        platforms: HashMap<String, ReleaseManifestPlatform>,
    },
}

/// Information about a release returned by the remote update server.
///
/// This type can have one of two shapes: Server Format (Dynamic Format) and Static Format.
#[derive(Debug, Clone)]
pub struct RemoteRelease {
    /// Version to install.
    pub version: Version,
    /// Release notes.
    pub notes: Option<String>,
    /// Release date.
    pub pub_date: Option<OffsetDateTime>,
    /// Release data.
    pub data: RemoteReleaseInner,
}

impl RemoteRelease {
    /// The release's download URL for the given target.
    pub fn download_url(&self, target: &str) -> Result<&Url> {
        match self.data {
            RemoteReleaseInner::Dynamic(ref platform) => Ok(&platform.url),
            RemoteReleaseInner::Static { ref platforms } => platforms
                .get(target)
                .map_or(Err(Error::TargetNotFound(target.to_string())), |p| {
                    Ok(&p.url)
                }),
        }
    }

    /// The release's signature for the given target.
    pub fn signature(&self, target: &str) -> Result<&String> {
        match self.data {
            RemoteReleaseInner::Dynamic(ref platform) => Ok(&platform.signature),
            RemoteReleaseInner::Static { ref platforms } => platforms
                .get(target)
                .map_or(Err(Error::TargetNotFound(target.to_string())), |platform| {
                    Ok(&platform.signature)
                }),
        }
    }
}

pub type OnBeforeExit = Arc<dyn Fn() + Send + Sync + 'static>;
pub type VersionComparator = Arc<dyn Fn(Version, RemoteRelease) -> bool + Send + Sync>;

pub struct UpdaterBuilder {
    app_name: String,
    current_version: Version,
    config: Config,
    pub(crate) version_comparator: Option<VersionComparator>,
    executable_path: Option<PathBuf>,
    target: Option<String>,
    endpoints: Option<Vec<Url>>,
    headers: HeaderMap,
    timeout: Option<Duration>,
    proxy: Option<Url>,
    installer_args: Vec<OsString>,
    current_exe_args: Vec<OsString>,
    on_before_exit: Option<OnBeforeExit>,
}

impl UpdaterBuilder {
    /// It's prefered to use [`crate::UpdaterExt::updater_builder`] instead of
    /// constructing a [`UpdaterBuilder`] with this function yourself
    pub fn new(app_name: String, current_version: Version, config: crate::Config) -> Self {
        Self {
            installer_args: config
                .windows
                .as_ref()
                .map(|w| w.installer_args.clone())
                .unwrap_or_default(),
            current_exe_args: Vec::new(),
            app_name,
            current_version,
            config,
            version_comparator: None,
            executable_path: None,
            target: None,
            endpoints: None,
            headers: Default::default(),
            timeout: None,
            proxy: None,
            on_before_exit: None,
        }
    }

    pub fn version_comparator<F: Fn(Version, RemoteRelease) -> bool + Send + Sync + 'static>(
        mut self,
        f: F,
    ) -> Self {
        self.version_comparator = Some(Arc::new(f));
        self
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target.replace(target.into());
        self
    }

    pub fn endpoints(mut self, endpoints: Vec<Url>) -> Result<Self> {
        crate::config::validate_endpoints(
            &endpoints,
            self.config.dangerous_insecure_transport_protocol,
        )?;

        self.endpoints.replace(endpoints);
        Ok(self)
    }

    pub fn executable_path<P: AsRef<Path>>(mut self, p: P) -> Self {
        self.executable_path.replace(p.as_ref().into());
        self
    }

    pub fn header<K, V>(mut self, key: K, value: V) -> Result<Self>
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        let key: std::result::Result<HeaderName, http::Error> = key.try_into().map_err(Into::into);
        let value: std::result::Result<HeaderValue, http::Error> =
            value.try_into().map_err(Into::into);
        self.headers.insert(key?, value?);

        Ok(self)
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    pub fn clear_headers(mut self) -> Self {
        self.headers.clear();
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn proxy(mut self, proxy: Url) -> Self {
        self.proxy.replace(proxy);
        self
    }

    pub fn pubkey<S: Into<String>>(mut self, pubkey: S) -> Self {
        self.config.pubkey = pubkey.into();
        self
    }

    pub fn installer_arg<S>(mut self, arg: S) -> Self
    where
        S: Into<OsString>,
    {
        self.installer_args.push(arg.into());
        self
    }

    pub fn installer_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args = args.into_iter().map(|a| a.into()).collect::<Vec<_>>();
        self.installer_args.extend_from_slice(&args);
        self
    }

    pub fn clear_installer_args(mut self) -> Self {
        self.installer_args.clear();
        self
    }

    pub fn on_before_exit<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.on_before_exit.replace(Arc::new(f));
        self
    }

    pub fn build(self) -> Result<Updater> {
        let endpoints = self
            .endpoints
            .unwrap_or_else(|| self.config.endpoints.clone());

        if endpoints.is_empty() {
            return Err(Error::EmptyEndpoints);
        };

        let arch = get_updater_arch().ok_or(Error::UnsupportedArch)?;
        let (target, json_target) = if let Some(target) = self.target {
            (target.clone(), target)
        } else {
            let target = get_updater_target().ok_or(Error::UnsupportedOs)?;
            (target.to_string(), format!("{target}-{arch}"))
        };

        let executable_path = self.executable_path.clone().unwrap_or(current_exe()?);

        // Get the extract_path from the provided executable_path
        let extract_path = if cfg!(target_os = "linux") {
            executable_path
        } else {
            extract_path_from_executable(&executable_path)?
        };

        Ok(Updater {
            config: self.config,
            app_name: self.app_name,
            current_version: self.current_version,
            version_comparator: self.version_comparator,
            timeout: self.timeout,
            proxy: self.proxy,
            endpoints,
            installer_args: self.installer_args,
            current_exe_args: self.current_exe_args,
            arch,
            target,
            json_target,
            headers: self.headers,
            extract_path,
            on_before_exit: self.on_before_exit,
        })
    }
}

impl UpdaterBuilder {
    pub(crate) fn current_exe_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args = args.into_iter().map(|a| a.into()).collect::<Vec<_>>();
        self.current_exe_args.extend_from_slice(&args);
        self
    }
}

pub struct Updater {
    config: Config,
    app_name: String,
    current_version: Version,
    version_comparator: Option<VersionComparator>,
    timeout: Option<Duration>,
    proxy: Option<Url>,
    endpoints: Vec<Url>,
    arch: &'static str,
    // The `{{target}}` variable we replace in the endpoint
    target: String,
    // The value we search if the updater server returns a JSON with the `platforms` object
    json_target: String,
    headers: HeaderMap,
    extract_path: PathBuf,
    on_before_exit: Option<OnBeforeExit>,
    #[allow(unused)]
    installer_args: Vec<OsString>,
    #[allow(unused)]
    current_exe_args: Vec<OsString>,
}

impl Updater {
    pub async fn check(&self) -> Result<Option<Update>> {
        // we want JSON only
        let mut headers = self.headers.clone();
        headers.insert("Accept", HeaderValue::from_str("application/json").unwrap());

        // Set SSL certs for linux if they aren't available.
        #[cfg(target_os = "linux")]
        {
            if std::env::var_os("SSL_CERT_FILE").is_none() {
                std::env::set_var("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt");
            }
            if std::env::var_os("SSL_CERT_DIR").is_none() {
                std::env::set_var("SSL_CERT_DIR", "/etc/ssl/certs");
            }
        }

        let mut remote_release: Option<RemoteRelease> = None;
        let mut last_error: Option<Error> = None;
        for url in &self.endpoints {
            // replace {{current_version}}, {{target}} and {{arch}} in the provided URL
            // this is useful if we need to query example
            // https://releases.myapp.com/update/{{target}}/{{arch}}/{{current_version}}
            // will be translated into ->
            // https://releases.myapp.com/update/darwin/aarch64/1.0.0
            // The main objective is if the update URL is defined via the Cargo.toml
            // the URL will be generated dynamically
            let version = self.current_version.to_string();
            let version = version.as_bytes();
            const CONTROLS_ADD: &AsciiSet = &CONTROLS.add(b'+');
            let encoded_version = percent_encoding::percent_encode(version, CONTROLS_ADD);
            let encoded_version = encoded_version.to_string();

            let url: Url = url
                .to_string()
                // url::Url automatically url-encodes the path components
                .replace("%7B%7Bcurrent_version%7D%7D", &encoded_version)
                .replace("%7B%7Btarget%7D%7D", &self.target)
                .replace("%7B%7Barch%7D%7D", self.arch)
                // but not query parameters
                .replace("{{current_version}}", &encoded_version)
                .replace("{{target}}", &self.target)
                .replace("{{arch}}", self.arch)
                .parse()?;

            let mut request = ClientBuilder::new().user_agent(UPDATER_USER_AGENT);
            if let Some(timeout) = self.timeout {
                request = request.timeout(timeout);
            }
            if let Some(ref proxy) = self.proxy {
                let proxy = reqwest::Proxy::all(proxy.as_str())?;
                request = request.proxy(proxy);
            }
            let response = request
                .build()?
                .get(url)
                .headers(headers.clone())
                .send()
                .await;

            if let Ok(res) = response {
                if res.status().is_success() {
                    // no updates found!
                    if StatusCode::NO_CONTENT == res.status() {
                        return Ok(None);
                    };

                    match serde_json::from_value::<RemoteRelease>(res.json().await?)
                        .map_err(Into::into)
                    {
                        Ok(release) => {
                            last_error = None;
                            remote_release = Some(release);
                            // we found a relase, break the loop
                            break;
                        }
                        Err(err) => last_error = Some(err),
                    }
                }
            }
        }

        // Last error is cleaned on success.
        // Shouldn't be triggered if we had a successfull call
        if let Some(error) = last_error {
            return Err(error);
        }

        // Extracted remote metadata
        let release = remote_release.ok_or(Error::ReleaseNotFound)?;

        let should_update = match self.version_comparator.as_ref() {
            Some(comparator) => comparator(self.current_version.clone(), release.clone()),
            None => release.version > self.current_version,
        };

        let update = if should_update {
            Some(Update {
                config: self.config.clone(),
                on_before_exit: self.on_before_exit.clone(),
                app_name: self.app_name.clone(),
                current_version: self.current_version.to_string(),
                target: self.target.clone(),
                extract_path: self.extract_path.clone(),
                version: release.version.to_string(),
                date: release.pub_date,
                download_url: release.download_url(&self.json_target)?.to_owned(),
                body: release.notes.clone(),
                signature: release.signature(&self.json_target)?.to_owned(),
                timeout: self.timeout,
                proxy: self.proxy.clone(),
                headers: self.headers.clone(),
                installer_args: self.installer_args.clone(),
                current_exe_args: self.current_exe_args.clone(),
            })
        } else {
            None
        };

        Ok(update)
    }
}

#[derive(Clone)]
pub struct Update {
    config: Config,
    #[allow(unused)]
    on_before_exit: Option<OnBeforeExit>,
    /// Update description
    pub body: Option<String>,
    /// Version used to check for update
    pub current_version: String,
    /// Version announced
    pub version: String,
    /// Update publish date
    pub date: Option<OffsetDateTime>,
    /// Target
    pub target: String,
    /// Download URL announced
    pub download_url: Url,
    /// Signature announced
    pub signature: String,
    /// Request timeout
    pub timeout: Option<Duration>,
    /// Request proxy
    pub proxy: Option<Url>,
    /// Request headers
    pub headers: HeaderMap,
    /// Extract path
    #[allow(unused)]
    extract_path: PathBuf,
    /// App name, used for creating named tempfiles on Windows
    #[allow(unused)]
    app_name: String,
    #[allow(unused)]
    installer_args: Vec<OsString>,
    #[allow(unused)]
    current_exe_args: Vec<OsString>,
}

impl Resource for Update {}

impl Update {
    /// Downloads the updater package, verifies it then return it as bytes.
    ///
    /// Use [`Update::install`] to install it
    pub async fn download<C: FnMut(usize, Option<u64>), D: FnOnce()>(
        &self,
        mut on_chunk: C,
        on_download_finish: D,
    ) -> Result<Vec<u8>> {
        // set our headers
        let mut headers = self.headers.clone();
        headers.insert(
            "Accept",
            HeaderValue::from_str("application/octet-stream").unwrap(),
        );

        let mut request = ClientBuilder::new().user_agent(UPDATER_USER_AGENT);
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        if let Some(ref proxy) = self.proxy {
            let proxy = reqwest::Proxy::all(proxy.as_str())?;
            request = request.proxy(proxy);
        }
        let response = request
            .build()?
            .get(self.download_url.clone())
            .headers(headers)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Error::Network(format!(
                "Download request failed with status: {}",
                response.status()
            )));
        }

        let content_length: Option<u64> = response
            .headers()
            .get("Content-Length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());

        let mut buffer = Vec::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            on_chunk(chunk.len(), content_length);
            buffer.extend(chunk);
        }
        on_download_finish();

        verify_signature(&buffer, &self.signature, &self.config.pubkey)?;

        Ok(buffer)
    }

    /// Installs the updater package downloaded by [`Update::download`]
    pub fn install(&self, bytes: impl AsRef<[u8]>) -> Result<()> {
        self.install_inner(bytes.as_ref())
    }

    /// Downloads and installs the updater package
    pub async fn download_and_install<C: FnMut(usize, Option<u64>), D: FnOnce()>(
        &self,
        on_chunk: C,
        on_download_finish: D,
    ) -> Result<()> {
        let bytes = self.download(on_chunk, on_download_finish).await?;
        self.install(bytes)
    }

    #[cfg(mobile)]
    fn install_inner(&self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
enum WindowsUpdaterType {
    Nsis {
        path: PathBuf,
        #[allow(unused)]
        temp: Option<tempfile::TempPath>,
    },
    Msi {
        path: PathBuf,
        #[allow(unused)]
        temp: Option<tempfile::TempPath>,
    },
}

#[cfg(windows)]
impl WindowsUpdaterType {
    fn nsis(path: PathBuf, temp: Option<tempfile::TempPath>) -> Self {
        Self::Nsis { path, temp }
    }

    fn msi(path: PathBuf, temp: Option<tempfile::TempPath>) -> Self {
        Self::Msi {
            path: path.wrap_in_quotes(),
            temp,
        }
    }
}

#[cfg(windows)]
impl Config {
    fn install_mode(&self) -> crate::config::WindowsUpdateInstallMode {
        self.windows
            .as_ref()
            .map(|w| w.install_mode.clone())
            .unwrap_or_default()
    }
}

/// Windows
#[cfg(windows)]
impl Update {
    /// ### Expected structure:
    /// ├── [AppName]_[version]_x64.msi              # Application MSI
    /// ├── [AppName]_[version]_x64-setup.exe        # NSIS installer
    /// ├── [AppName]_[version]_x64.msi.zip          # ZIP generated by tauri-bundler
    /// │   └──[AppName]_[version]_x64.msi           # Application MSI
    /// ├── [AppName]_[version]_x64-setup.exe.zip          # ZIP generated by tauri-bundler
    /// │   └──[AppName]_[version]_x64-setup.exe           # NSIS installer
    /// └── ...
    fn install_inner(&self, bytes: &[u8]) -> Result<()> {
        use std::iter::once;
        use windows_sys::{
            w,
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOW},
        };

        let updater_type = self.extract(bytes)?;

        let install_mode = self.config.install_mode();
        let current_args = &self.current_exe_args()[1..];
        let msi_args;

        let installer_args: Vec<&OsStr> = match &updater_type {
            WindowsUpdaterType::Nsis { .. } => install_mode
                .nsis_args()
                .iter()
                .map(OsStr::new)
                .chain(once(OsStr::new("/UPDATE")))
                .chain(once(OsStr::new("/ARGS")))
                .chain(current_args.to_vec())
                .chain(self.installer_args())
                .collect(),
            WindowsUpdaterType::Msi { path, .. } => {
                let escaped_args = current_args
                    .iter()
                    .map(escape_msi_property_arg)
                    .collect::<Vec<_>>()
                    .join(" ");
                msi_args = OsString::from(format!("LAUNCHAPPARGS=\"{escaped_args}\""));

                [OsStr::new("/i"), path.as_os_str()]
                    .into_iter()
                    .chain(install_mode.msiexec_args().iter().map(OsStr::new))
                    .chain(once(OsStr::new("/promptrestart")))
                    .chain(self.installer_args())
                    .chain(once(OsStr::new("AUTOLAUNCHAPP=True")))
                    .chain(once(msi_args.as_os_str()))
                    .collect()
            }
        };

        if let Some(on_before_exit) = self.on_before_exit.as_ref() {
            on_before_exit();
        }

        let file = match &updater_type {
            WindowsUpdaterType::Nsis { path, .. } => path.as_os_str().to_os_string(),
            WindowsUpdaterType::Msi { .. } => std::env::var("SYSTEMROOT").as_ref().map_or_else(
                |_| OsString::from("msiexec.exe"),
                |p| OsString::from(format!("{p}\\System32\\msiexec.exe")),
            ),
        };
        let file = encode_wide(file);

        let parameters = installer_args.join(OsStr::new(" "));
        let parameters = encode_wide(parameters);

        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                w!("open"),
                file.as_ptr(),
                parameters.as_ptr(),
                std::ptr::null(),
                SW_SHOW,
            )
        };

        std::process::exit(0);
    }

    fn installer_args(&self) -> Vec<&OsStr> {
        self.installer_args
            .iter()
            .map(OsStr::new)
            .collect::<Vec<_>>()
    }

    fn current_exe_args(&self) -> Vec<&OsStr> {
        self.current_exe_args
            .iter()
            .map(OsStr::new)
            .collect::<Vec<_>>()
    }

    fn extract(&self, bytes: &[u8]) -> Result<WindowsUpdaterType> {
        #[cfg(feature = "zip")]
        if infer::archive::is_zip(bytes) {
            return self.extract_zip(bytes);
        }

        self.extract_exe(bytes)
    }

    fn make_temp_dir(&self) -> Result<PathBuf> {
        Ok(tempfile::Builder::new()
            .prefix(&format!("{}-{}-updater-", self.app_name, self.version))
            .tempdir()?
            .into_path())
    }

    #[cfg(feature = "zip")]
    fn extract_zip(&self, bytes: &[u8]) -> Result<WindowsUpdaterType> {
        let temp_dir = self.make_temp_dir()?;

        let archive = Cursor::new(bytes);
        let mut extractor = zip::ZipArchive::new(archive)?;
        extractor.extract(&temp_dir)?;

        let paths = std::fs::read_dir(&temp_dir)?;
        for path in paths {
            let path = path?.path();
            let ext = path.extension();
            if ext == Some(OsStr::new("exe")) {
                return Ok(WindowsUpdaterType::nsis(path, None));
            } else if ext == Some(OsStr::new("msi")) {
                return Ok(WindowsUpdaterType::msi(path, None));
            }
        }

        Err(crate::Error::BinaryNotFoundInArchive)
    }

    fn extract_exe(&self, bytes: &[u8]) -> Result<WindowsUpdaterType> {
        if infer::app::is_exe(bytes) {
            let (path, temp) = self.write_to_temp(bytes, ".exe")?;
            Ok(WindowsUpdaterType::nsis(path, temp))
        } else if infer::archive::is_msi(bytes) {
            let (path, temp) = self.write_to_temp(bytes, ".msi")?;
            Ok(WindowsUpdaterType::msi(path, temp))
        } else {
            Err(crate::Error::InvalidUpdaterFormat)
        }
    }

    fn write_to_temp(
        &self,
        bytes: &[u8],
        ext: &str,
    ) -> Result<(PathBuf, Option<tempfile::TempPath>)> {
        use std::io::Write;

        let temp_dir = self.make_temp_dir()?;
        let mut temp_file = tempfile::Builder::new()
            .prefix(&format!("{}-{}-installer", self.app_name, self.version))
            .suffix(ext)
            .rand_bytes(0)
            .tempfile_in(temp_dir)?;
        temp_file.write_all(bytes)?;

        let temp = temp_file.into_temp_path();
        Ok((temp.to_path_buf(), Some(temp)))
    }
}

/// Linux (AppImage and Deb)
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
impl Update {
    /// ### Expected structure:
    /// ├── [AppName]_[version]_amd64.AppImage.tar.gz    # GZ generated by tauri-bundler
    /// │   └──[AppName]_[version]_amd64.AppImage        # Application AppImage
    /// ├── [AppName]_[version]_amd64.deb                # Debian package
    /// └── ...
    ///
    fn install_inner(&self, bytes: &[u8]) -> Result<()> {
        if self.is_deb_package() {
            self.install_deb(bytes)
        } else {
            // Handle AppImage or other formats
            self.install_appimage(bytes)
        }
    }

    fn install_appimage(&self, bytes: &[u8]) -> Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let extract_path_metadata = self.extract_path.metadata()?;

        let tmp_dir_locations = vec![
            Box::new(|| Some(std::env::temp_dir())) as Box<dyn FnOnce() -> Option<PathBuf>>,
            Box::new(dirs::cache_dir),
            Box::new(|| Some(self.extract_path.parent().unwrap().to_path_buf())),
        ];

        for tmp_dir_location in tmp_dir_locations {
            if let Some(tmp_dir_location) = tmp_dir_location() {
                let tmp_dir = tempfile::Builder::new()
                    .prefix("tauri_current_app")
                    .tempdir_in(tmp_dir_location)?;
                let tmp_dir_metadata = tmp_dir.path().metadata()?;

                if extract_path_metadata.dev() == tmp_dir_metadata.dev() {
                    let mut perms = tmp_dir_metadata.permissions();
                    perms.set_mode(0o700);
                    std::fs::set_permissions(tmp_dir.path(), perms)?;

                    let tmp_app_image = &tmp_dir.path().join("current_app.AppImage");

                    let permissions = std::fs::metadata(&self.extract_path)?.permissions();

                    // create a backup of our current app image
                    std::fs::rename(&self.extract_path, tmp_app_image)?;

                    #[cfg(feature = "zip")]
                    if infer::archive::is_gz(bytes) {
                        // extract the buffer to the tmp_dir
                        // we extract our signed archive into our final directory without any temp file
                        let archive = Cursor::new(bytes);
                        let decoder = flate2::read::GzDecoder::new(archive);
                        let mut archive = tar::Archive::new(decoder);
                        for mut entry in archive.entries()?.flatten() {
                            if let Ok(path) = entry.path() {
                                if path.extension() == Some(OsStr::new("AppImage")) {
                                    // if something went wrong during the extraction, we should restore previous app
                                    if let Err(err) = entry.unpack(&self.extract_path) {
                                        std::fs::rename(tmp_app_image, &self.extract_path)?;
                                        return Err(err.into());
                                    }
                                    // early finish we have everything we need here
                                    return Ok(());
                                }
                            }
                        }
                        // if we have not returned early we should restore the backup
                        std::fs::rename(tmp_app_image, &self.extract_path)?;
                        return Err(Error::BinaryNotFoundInArchive);
                    }

                    return match std::fs::write(&self.extract_path, bytes)
                        .and_then(|_| std::fs::set_permissions(&self.extract_path, permissions))
                    {
                        Err(err) => {
                            // if something went wrong during the extraction, we should restore previous app
                            std::fs::rename(tmp_app_image, &self.extract_path)?;
                            Err(err.into())
                        }
                        Ok(_) => Ok(()),
                    };
                }
            }
        }

        Err(Error::TempDirNotOnSameMountPoint)
    }

    fn is_deb_package(&self) -> bool {
        // First check if we're in a typical Debian installation path
        let in_system_path = self
            .extract_path
            .to_str()
            .map(|p| p.starts_with("/usr"))
            .unwrap_or(false);

        if !in_system_path {
            return false;
        }

        // Then verify it's actually a Debian-based system by checking for dpkg
        let dpkg_exists = std::path::Path::new("/var/lib/dpkg").exists();
        let apt_exists = std::path::Path::new("/etc/apt").exists();

        // Additional check for the package in dpkg database
        let package_in_dpkg = if let Ok(output) = std::process::Command::new("dpkg")
            .args(["-S", &self.extract_path.to_string_lossy()])
            .output()
        {
            output.status.success()
        } else {
            false
        };

        // Consider it a deb package only if:
        // 1. We're in a system path AND
        // 2. We have Debian package management tools AND
        // 3. The binary is tracked by dpkg
        dpkg_exists && apt_exists && package_in_dpkg
    }

    fn install_deb(&self, bytes: &[u8]) -> Result<()> {
        // First verify the bytes are actually a .deb package
        if !infer::archive::is_deb(bytes) {
            return Err(Error::InvalidUpdaterFormat);
        }

        // Try different temp directories
        let tmp_dir_locations = vec![
            Box::new(|| Some(std::env::temp_dir())) as Box<dyn FnOnce() -> Option<PathBuf>>,
            Box::new(dirs::cache_dir),
            Box::new(|| Some(self.extract_path.parent().unwrap().to_path_buf())),
        ];

        // Try writing to multiple temp locations until one succeeds
        for tmp_dir_location in tmp_dir_locations {
            if let Some(path) = tmp_dir_location() {
                if let Ok(tmp_dir) = tempfile::Builder::new()
                    .prefix("tauri_deb_update")
                    .tempdir_in(path)
                {
                    let deb_path = tmp_dir.path().join("package.deb");

                    // Try writing the .deb file
                    if std::fs::write(&deb_path, bytes).is_ok() {
                        // If write succeeds, proceed with installation
                        return self.try_install_with_privileges(&deb_path);
                    }
                    // If write fails, continue to next temp location
                }
            }
        }

        // If we get here, all temp locations failed
        Err(Error::TempDirNotFound)
    }

    fn try_install_with_privileges(&self, deb_path: &Path) -> Result<()> {
        // 1. First try using pkexec (graphical sudo prompt)
        if let Ok(status) = std::process::Command::new("pkexec")
            .arg("dpkg")
            .arg("-i")
            .arg(deb_path)
            .status()
        {
            if status.success() {
                return Ok(());
            }
        }

        // 2. Try zenity or kdialog for a graphical sudo experience
        if let Ok(password) = self.get_password_graphically() {
            if self.install_with_sudo(deb_path, &password)? {
                return Ok(());
            }
        }

        // 3. Final fallback: terminal sudo
        let status = std::process::Command::new("sudo")
            .arg("dpkg")
            .arg("-i")
            .arg(deb_path)
            .status()?;

        if status.success() {
            Ok(())
        } else {
            Err(Error::DebInstallFailed)
        }
    }

    fn get_password_graphically(&self) -> Result<String> {
        // Try zenity first
        let zenity_result = std::process::Command::new("zenity")
            .args([
                "--password",
                "--title=Authentication Required",
                "--text=Enter your password to install the update:",
            ])
            .output();

        if let Ok(output) = zenity_result {
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }

        // Fall back to kdialog if zenity fails or isn't available
        let kdialog_result = std::process::Command::new("kdialog")
            .args(["--password", "Enter your password to install the update:"])
            .output();

        if let Ok(output) = kdialog_result {
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }

        Err(Error::AuthenticationFailed)
    }

    fn install_with_sudo(&self, deb_path: &Path, password: &str) -> Result<bool> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new("sudo")
            .arg("-S") // read password from stdin
            .arg("dpkg")
            .arg("-i")
            .arg(deb_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            // Write password to stdin
            writeln!(stdin, "{}", password)?;
        }

        let status = child.wait()?;
        Ok(status.success())
    }
}

/// MacOS
#[cfg(target_os = "macos")]
impl Update {
    /// ### Expected structure:
    /// ├── [AppName]_[version]_x64.app.tar.gz       # GZ generated by tauri-bundler
    /// │   └──[AppName].app                         # Main application
    /// │      └── Contents                          # Application contents...
    /// │          └── ...
    /// └── ...
    fn install_inner(&self, bytes: &[u8]) -> Result<()> {
        use flate2::read::GzDecoder;

        let cursor = Cursor::new(bytes);
        let mut extracted_files: Vec<PathBuf> = Vec::new();

        // the first file in the tar.gz will always be
        // <app_name>/Contents
        let tmp_dir = tempfile::Builder::new()
            .prefix("tauri_current_app")
            .tempdir()?;

        // create backup of our current app
        std::fs::rename(&self.extract_path, tmp_dir.path())?;

        let decoder = GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(decoder);

        std::fs::create_dir(&self.extract_path)?;

        for entry in archive.entries()? {
            let mut entry = entry?;

            // skip the first folder (should be the app name)
            let collected_path: PathBuf = entry.path()?.iter().skip(1).collect();
            let extraction_path = &self.extract_path.join(collected_path);

            // if something went wrong during the extraction, we should restore previous app
            if let Err(err) = entry.unpack(extraction_path) {
                for file in extracted_files.iter().rev() {
                    // delete all the files we extracted
                    if file.is_dir() {
                        std::fs::remove_dir(file)?;
                    } else {
                        std::fs::remove_file(file)?;
                    }
                }
                std::fs::rename(tmp_dir.path(), &self.extract_path)?;
                return Err(err.into());
            }

            extracted_files.push(extraction_path.to_path_buf());
        }

        let _ = std::process::Command::new("touch")
            .arg(&self.extract_path)
            .status();

        Ok(())
    }
}

/// Gets the target string used on the updater.
pub fn target() -> Option<String> {
    if let (Some(target), Some(arch)) = (get_updater_target(), get_updater_arch()) {
        Some(format!("{target}-{arch}"))
    } else {
        None
    }
}

pub(crate) fn get_updater_target() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some("linux")
    } else if cfg!(target_os = "macos") {
        // TODO shouldn't this be macos instead?
        Some("darwin")
    } else if cfg!(target_os = "windows") {
        Some("windows")
    } else {
        None
    }
}

pub(crate) fn get_updater_arch() -> Option<&'static str> {
    if cfg!(target_arch = "x86") {
        Some("i686")
    } else if cfg!(target_arch = "x86_64") {
        Some("x86_64")
    } else if cfg!(target_arch = "arm") {
        Some("armv7")
    } else if cfg!(target_arch = "aarch64") {
        Some("aarch64")
    } else {
        None
    }
}

pub fn extract_path_from_executable(executable_path: &Path) -> Result<PathBuf> {
    // Return the path of the current executable by default
    // Example C:\Program Files\My App\
    let extract_path = executable_path
        .parent()
        .map(PathBuf::from)
        .ok_or(Error::FailedToDetermineExtractPath)?;

    // MacOS example binary is in /Applications/TestApp.app/Contents/MacOS/myApp
    // We need to get /Applications/<app>.app
    // TODO(lemarier): Need a better way here
    // Maybe we could search for <*.app> to get the right path
    #[cfg(target_os = "macos")]
    if extract_path
        .display()
        .to_string()
        .contains("Contents/MacOS")
    {
        return extract_path
            .parent()
            .map(PathBuf::from)
            .ok_or(Error::FailedToDetermineExtractPath)?
            .parent()
            .map(PathBuf::from)
            .ok_or(Error::FailedToDetermineExtractPath);
    }

    Ok(extract_path)
}

impl<'de> Deserialize<'de> for RemoteRelease {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct InnerRemoteRelease {
            #[serde(alias = "name", deserialize_with = "parse_version")]
            version: Version,
            notes: Option<String>,
            pub_date: Option<String>,
            platforms: Option<HashMap<String, ReleaseManifestPlatform>>,
            // dynamic platform response
            url: Option<Url>,
            signature: Option<String>,
        }

        let release = InnerRemoteRelease::deserialize(deserializer)?;

        let pub_date = if let Some(date) = release.pub_date {
            Some(
                OffsetDateTime::parse(&date, &time::format_description::well_known::Rfc3339)
                    .map_err(|e| DeError::custom(format!("invalid value for `pub_date`: {e}")))?,
            )
        } else {
            None
        };

        Ok(RemoteRelease {
            version: release.version,
            notes: release.notes,
            pub_date,
            data: if let Some(platforms) = release.platforms {
                RemoteReleaseInner::Static { platforms }
            } else {
                RemoteReleaseInner::Dynamic(ReleaseManifestPlatform {
                    url: release.url.ok_or_else(|| {
                        DeError::custom("the `url` field was not set on the updater response")
                    })?,
                    signature: release.signature.ok_or_else(|| {
                        DeError::custom("the `signature` field was not set on the updater response")
                    })?,
                })
            },
        })
    }
}

fn parse_version<'de, D>(deserializer: D) -> std::result::Result<Version, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let str = String::deserialize(deserializer)?;

    Version::from_str(str.trim_start_matches('v')).map_err(serde::de::Error::custom)
}

// Validate signature
fn verify_signature(data: &[u8], release_signature: &str, pub_key: &str) -> Result<bool> {
    // we need to convert the pub key
    let pub_key_decoded = base64_to_string(pub_key)?;
    let public_key = PublicKey::decode(&pub_key_decoded)?;
    let signature_base64_decoded = base64_to_string(release_signature)?;
    let signature = Signature::decode(&signature_base64_decoded)?;

    // Validate signature or bail out
    public_key.verify(data, &signature, true)?;
    Ok(true)
}

fn base64_to_string(base64_string: &str) -> Result<String> {
    let decoded_string = &base64::engine::general_purpose::STANDARD.decode(base64_string)?;
    let result = std::str::from_utf8(decoded_string)
        .map_err(|_| Error::SignatureUtf8(base64_string.into()))?
        .to_string();
    Ok(result)
}

#[cfg(windows)]
fn encode_wide(string: impl AsRef<OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    string
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
trait PathExt {
    fn wrap_in_quotes(&self) -> Self;
}

#[cfg(windows)]
impl PathExt for PathBuf {
    fn wrap_in_quotes(&self) -> Self {
        let mut msi_path = OsString::from("\"");
        msi_path.push(self.as_os_str());
        msi_path.push("\"");
        PathBuf::from(msi_path)
    }
}

#[cfg(windows)]
fn escape_msi_property_arg(arg: impl AsRef<OsStr>) -> String {
    let mut arg = arg.as_ref().to_string_lossy().to_string();

    // Otherwise this argument will get lost in ShellExecute
    if arg.is_empty() {
        return "\"\"\"\"".to_string();
    } else if !arg.contains(' ') && !arg.contains('"') {
        return arg;
    }

    if arg.contains('"') {
        arg = arg.replace('"', r#""""""#)
    }

    if arg.starts_with('-') {
        if let Some((a1, a2)) = arg.split_once('=') {
            format!("{a1}=\"\"{a2}\"\"")
        } else {
            format!("\"\"{arg}\"\"")
        }
    } else {
        format!("\"\"{arg}\"\"")
    }
}

#[cfg(test)]
mod tests {

    #[test]
    #[cfg(windows)]
    fn it_wraps_correctly() {
        use super::PathExt;
        use std::path::PathBuf;

        assert_eq!(
            PathBuf::from("C:\\Users\\Some User\\AppData\\tauri-example.exe").wrap_in_quotes(),
            PathBuf::from("\"C:\\Users\\Some User\\AppData\\tauri-example.exe\"")
        )
    }

    #[test]
    #[cfg(windows)]
    fn it_escapes_correctly() {
        use crate::updater::escape_msi_property_arg;

        // Explanation for quotes:
        // The output of escape_msi_property_args() will be used in `LAUNCHAPPARGS=\"{HERE}\"`. This is the first quote level.
        // To escape a quotation mark we use a second quotation mark, so "" is interpreted as " later.
        // This means that the escaped strings can't ever have a single quotation mark!
        // Now there are 3 major things to look out for to not break the msiexec call:
        //   1) Wrap spaces in quotation marks, otherwise it will be interpreted as the end of the msiexec argument.
        //   2) Escape escaping quotation marks, otherwise they will either end the msiexec argument or be ignored.
        //   3) Escape emtpy args in quotation marks, otherwise the argument will get lost.
        let cases = [
            "something",
            "--flag",
            "--empty=",
            "--arg=value",
            "some space",                     // This simulates `./my-app "some string"`.
            "--arg value", // -> This simulates `./my-app "--arg value"`. Same as above but it triggers the startsWith(`-`) logic.
            "--arg=unwrapped space", // `./my-app --arg="unwrapped space"`
            "--arg=\"wrapped\"", // `./my-app --args=""wrapped""`
            "--arg=\"wrapped space\"", // `./my-app --args=""wrapped space""`
            "--arg=midword\"wrapped space\"", // `./my-app --args=midword""wrapped""`
            "",            // `./my-app '""'`
        ];
        let cases_escaped = [
            "something",
            "--flag",
            "--empty=",
            "--arg=value",
            "\"\"some space\"\"",
            "\"\"--arg value\"\"",
            "--arg=\"\"unwrapped space\"\"",
            r#"--arg=""""""wrapped"""""""#,
            r#"--arg=""""""wrapped space"""""""#,
            r#"--arg=""midword""""wrapped space"""""""#,
            "\"\"\"\"",
        ];

        // Just to be sure we didn't mess that up
        assert_eq!(cases.len(), cases_escaped.len());

        for (orig, escaped) in cases.iter().zip(cases_escaped) {
            assert_eq!(escape_msi_property_arg(orig), escaped);
        }
    }
}
