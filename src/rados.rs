// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Rust bindings for the [librados](https://docs.ceph.com/en/latest/rados/api/librados/#librados-c) C library
//! for interacting with a Ceph cluster.
//!
//! The main types this is done through are the [`RadosClient`] and the [`IoCtx`]. The former is a handle to
//! an entire Ceph cluster, while the latter is used to perform operations on a particular pool.

use std::{
    collections::HashSet,
    ffi::{CString, NulError, OsStr, OsString},
    io::ErrorKind,
    os::raw::c_void,
    path::Path,
    ptr::{self, NonNull},
    sync::Mutex,
};

use rados_sys::{
    rados_conf_parse_argv_remainder,
    rados_conf_parse_env,
    rados_conf_read_file,
    rados_conf_set,
    rados_connect,
    rados_create,
    rados_ioctx_create,
    rados_shutdown,
    rados_t,
};

use crate::{
    async_rt::{DynExecutor, Executor},
    util::{ArcWait, TryIntoCString, check_os_error},
};

mod io_ctx;
pub use io_ctx::IoCtx;
pub(crate) use io_ctx::{IoCtxHandle, IoCtxPtr};

/// The version of librados headers used to generate the system bindings that this
/// crate builds on.
///
/// Note that this is unrelated to the Ceph version or the `async-ceph` version.
pub const VERSION: crate::LibVersion = crate::LibVersion {
    major: rados_sys::LIBRADOS_VER_MAJOR,
    minor: rados_sys::LIBRADOS_VER_MINOR,
    patch: rados_sys::LIBRADOS_VER_EXTRA,
};

/// Returns the current runtime version of librados.
///
/// Note that this is unrelated to the Ceph version or the `async-ceph` version.
///
/// ```no_run
/// let rados_version = async_ceph::rados::version();
/// println!("RADOS version: {rados_version}");
/// ```
pub fn version() -> crate::LibVersion {
    let (mut major, mut minor, mut patch) = (0, 0, 0);
    unsafe { rados_sys::rados_version(&mut major, &mut minor, &mut patch) };
    crate::LibVersion {
        major: major as u32,
        minor: minor as u32,
        patch: patch as u32,
    }
}

#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct RadosPtr(NonNull<c_void>);

unsafe impl Send for RadosPtr {}
unsafe impl Sync for RadosPtr {}

impl RadosPtr {
    pub fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for RadosPtr {
    #[tracing::instrument("RadosPtr::drop", level = "debug")]
    fn drop(&mut self) {
        unsafe { rados_shutdown(self.as_ptr()) };
    }
}

/// Refcounted owned handle to a [`RadosClient`].
pub(crate) type RadosClientHandle = ArcWait<RadosPtr>;

/// RADOS client.
///
/// This is a high-level abstraction around a [`rados_t`] instance from
/// [librados](https://docs.ceph.com/en/latest/rados/api/librados/#librados-c). It is a handle to the
/// entire Ceph cluster.
///
/// Once a [`RadosClient`] is created, it typically isn't used besides calling
/// [`create_io_ctx`](RadosClient::create_io_ctx) to instantiate a pool handle (also known as an [IoCtx])
/// which exposes the majority of RADOS and RBD operations.
///
/// # Connecting
///
/// A client can be created using the `admin` user and default configuration through
/// [`RadosClient::new`]:
///
/// ```no_run
/// # #[cfg(feature = "tokio_rt")]
/// # async fn test() -> Result<(), Box<dyn std::error::Error>> {
/// use async_ceph::{rados::RadosClient, async_rt::Tokio};
///
/// let cluster = RadosClient::new(&Tokio).await?;
/// println!("connected! fsid: {}", cluster.fsid()?);
///
/// # Ok(())
/// # }
/// ```
///
/// An [`Executor`] that the client and any child objects use to spawn blocking tasks must be provided.
/// If using the Tokio runtime, as in the above example, the `tokio_rt` feature provides such an implementation
/// for the [`Runtime`](tokio::runtime::Runtime) struct as well as the [`crate::async_rt::Tokio`] marker type
/// deferring to the current runtime instance.
///
/// To manually configure the connection to the cluster, use the [`RadosClientBuilder`]:
///
/// ```no_run
/// # #[cfg(feature = "tokio_rt")]
/// # async fn test() -> Result<(), Box<dyn std::error::Error>> {
/// use async_ceph::{rados::RadosClientBuilder, async_rt::Tokio};
///
/// let cluster = RadosClientBuilder::new("username")?
///     // load options from a Ceph config file
///     .load_config_file("./ceph.conf")?
///     // load options in CLI notation from an environment variable
///     .load_env("EXTRA_CEPH_ARGS")?
///     // load options from *valid* CLI arguments
///     .load_cli_strict("--mon-host 1.2.3.4".split(' '))?
///     // set options key/values directly
///     .set("keyfile", "./path/to/key/file")?
///     .executor(&Tokio)
///     .connect()
///     .await?;
///
/// println!("connected! fsid: {}", cluster.fsid()?);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct RadosClient<'a> {
    handle: RadosClientHandle,
    executor: &'a dyn DynExecutor,
}

impl<'a> RadosClient<'a> {
    /// Create a [`RadosClient`] that will connect to the Ceph cluster described by the default
    /// configuration using the `admin` user.
    ///
    /// Configuration values will be pulled from the `CEPH_ARGS` environment variable, as well as the first
    /// config file that exists amongst:
    /// - `$CEPH_CONF` (environment variable)
    /// - `/etc/ceph/ceph.conf`
    /// - `~/.ceph/config`
    /// - `./ceph.conf`
    ///
    ///
    /// For more control over the Ceph configuration, use the [`RadosClientBuilder`] directly.
    ///
    /// The client will use the provided [`Executor`] to spawn async tasks that are needed internally.
    /// This can be a marker type like [`crate::async_rt::Tokio`] that defers to the current Tokio runtime
    /// context, or a reference to the runtime itself.
    pub async fn new(executor: &'a dyn DynExecutor) -> crate::Result<Self> {
        RadosClientBuilder::try_default()?.executor(executor).connect().await
    }

    #[cfg(feature = "cephfs")]
    /// Returns a handle to the underlying Rados client.
    pub(crate) fn get_handle(&self) -> RadosClientHandle {
        self.handle.clone()
    }

    #[cfg(feature = "cephfs")]
    /// Returns the underlying executor.
    pub(crate) fn get_executor(&self) -> &'a dyn DynExecutor {
        self.executor
    }

    /// Creates an [`IoCtx`]. This is required for virtually all operations that interact with the cluster.
    ///
    /// This is an asynchronous operation as librados will check that the pool actually exists before creating
    /// the [`IoCtx`].
    ///
    /// ```no_run
    /// # async fn test() -> Result<(), Box<dyn std::error::Error>> {
    /// # let client: async_ceph::rados::RadosClient<'static> = panic!();
    /// let pool = client.create_io_ctx("my-pool").await?;
    /// # }
    /// ```
    #[tracing::instrument("RadosClient::create_io_ctx", level = "debug")]
    pub async fn create_io_ctx<N>(&self, pool_name: N) -> crate::Result<IoCtx<'a>>
    where
        N: TryIntoCString + std::fmt::Debug, {
        let pool_name = pool_name.try_into_cstring().map_err(crate::Error::ffi)?;

        let handle = self.handle.clone();
        let ioctx = self
            .executor
            .spawn_blocking(move || unsafe {
                let mut ioctx = ptr::null_mut();
                check_os_error(rados_ioctx_create(handle.as_ptr(), pool_name.as_ptr(), &mut ioctx))?;
                let ioctx =
                    NonNull::new(ioctx).ok_or_else(|| crate::Error::unexpected("unexpected rados_ioctx_create failure"))?;

                // We need to create the owning IoCtxPtr here to avoid leaking if the future is cancelled
                Ok::<_, crate::Error>(IoCtxPtr::new(handle, ioctx))
            })
            .await
            .expect("failed to await blocking task")?;

        Ok(IoCtx::new(IoCtxHandle::new(ioctx), self.executor))
    }

    /// Returns the fsid of the cluster as a hexadecimal string.
    ///
    /// The fsid is a unique id for the entire Ceph cluster.
    ///
    /// ```no_run
    /// # let client: async_ceph::rados::RadosClient<'static> = panic!();
    /// println!("cluster fsid: {}", client.fsid()?);
    /// # Ok::<_, async_ceph::Error>(())
    /// ```
    pub fn fsid(&self) -> crate::Result<String> {
        // a fsid is 36 characters (37 with nul terminator), 40 here is just a nicely aligned number that's bigger.
        let fsid = unsafe {
            crate::util::get_unknown_size_cstr(|buf, size| rados_sys::rados_cluster_fsid(self.handle.as_ptr(), buf, size), 40)?
        };
        fsid.try_into()
            .map_err(|_| crate::Error::unexpected("fsid not convertible to utf8 string"))
    }

    /// Wait until all references to this client have been dropped and it has been shut down.
    ///
    ///
    /// # Deadlock warning
    ///
    /// Holding any owned [`IoCtx`] or [`Image`](crate::rbd::Image) obtained from this client
    /// past this `.await` point will lead to a deadlock!
    ///
    /// <div class="warning">
    ///
    /// ```no_run
    /// # async fn test() -> Result<(), Box<dyn std::error::Error>> {
    /// # use async_ceph::rados::RadosClient;
    /// # fn get_client() -> RadosClient<'static> { panic!() }
    /// let client: RadosClient<'static> = get_client();
    /// let pool = client.create_io_ctx("my-pool").await?;
    ///
    /// // do something with the pool...
    ///
    /// // Deadlocks: `pool` is still live when this is called!
    /// client.shutdown().await;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// </div>
    ///
    /// Instead, explicitly drop `pool` before calling `shutdown`:
    ///
    /// ```no_run
    /// # async fn test() -> Result<(), Box<dyn std::error::Error>> {
    /// # use async_ceph::rados::RadosClient;
    /// # fn get_client() -> RadosClient<'static> { panic!() }
    /// let client: RadosClient<'static> = get_client();
    /// let pool = client.create_io_ctx("my-pool").await?;
    ///
    /// // do something with the pool, then drop it
    /// drop(pool);
    ///
    /// // OK: `pool` has been dropped
    /// client.shutdown().await;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("RadosClient::shutdown", level = "debug")]
    pub async fn shutdown(mut self) {
        // TODO: Ideally we'd want deadlock detection if the above precondition is violated,
        // to help users debug. How to best implement this?
        ArcWait::wait_unique(&mut self.handle).await
    }

    /// Blocks until all references to this [`RadosClient`] have been dropped, then shuts it down.
    ///
    /// If in an async context, prefer [`RadosClient::shutdown`].
    ///
    /// # Deadlock warning
    ///
    /// Holding any owned [`IoCtx`] or [`Image`](crate::rbd::Image) past this point will lead to a
    /// deadlock!
    #[tracing::instrument("RadosClient::shutdown_blocking", level = "debug")]
    pub fn shutdown_blocking(mut self) {
        // TODO: Ideally we'd want deadlock detection if the above precondition is violated,
        // to help users debug. How to best implement this?
        ArcWait::blocking_wait_unique(&mut self.handle)
    }
}

/// Builder struct for connecting to a Ceph cluster and creating a [`RadosClient`] instance.
///
/// This builder has two mandatory parameters:
/// - The user to connect to the cluster as. This is specified on creation via [`RadosClientBuilder::new`].
///   One can use [`RadosClientBuilder::try_default`] or the [`Default`] impl to connect as the
///   `"admin"` user.
///
/// - The [`Executor`] that will be used by the [`RadosClient`] to spawn async tasks. This is
///   specified using [`RadosClientBuilder::executor`].
///
/// Before [`RadosClientBuilder::connect`] can be called, an executor *must* be provided using
/// the [`executor`](Self::executor) builder method.
#[derive(Debug)]
pub struct RadosClientBuilder<'a, E: ?Sized + 'a = dyn DynExecutor + 'a> {
    cluster: RadosPtr,
    executor: &'a E,
}

/// Marker type indicating that no executor was specified for the [`RadosClientBuilder`].
#[derive(Debug)]
pub struct NoExecutor;

/// Error type specific to [`RadosClientBuilder`] failures.
#[derive(Debug, thiserror::Error)]
pub enum RadosClientBuilderError {
    /// An invalid RADOS option was passed to [`RadosClientBuilder::set`].
    #[error("not a valid rados setting: {0}")]
    InvalidOption(String),

    /// A configuration entry contains NUL.
    ///
    /// ```
    /// # use async_ceph::rados::*;
    /// #
    /// let result = RadosClientBuilder::default().set("key", "foo\0bar");
    ///
    /// assert!(matches!(result, Err(RadosClientBuilderError::InvalidString(_))));
    /// ```
    #[error("configuration entry contains NUL")]
    InvalidString(#[from] NulError),

    /// Some CLI args passed to [`RadosClientBuilder::load_cli_strict`] were not recognized.
    #[error("cli args not recognized by librados: {0:?}")]
    UnrecognizedCliArgs(Vec<OsString>),

    /// A librados ffi call returned an error.
    #[error("RADOS error: {0:?}")]
    Rados(#[from] crate::Error),
}

impl<'a> RadosClientBuilder<'a, NoExecutor> {
    /// Create a [`RadosClientBuilder`], connecting as the provided
    /// [user id](https://docs.ceph.com/en/latest/rados/operations/user-management/#command-line-usage).
    ///
    /// Configuration values will be pulled from the `CEPH_ARGS` environment variable, as well as the first
    /// config file that exists amongst:
    /// - `$CEPH_CONF` (environment variable)
    /// - `/etc/ceph/ceph.conf`
    /// - `~/.ceph/config`
    /// - `./ceph.conf`
    ///
    /// # Fails
    /// - if librados returns an error when creating the cluster handle.
    pub fn new(user: impl TryIntoCString) -> crate::Result<Self> {
        let user = user.try_into_cstring().map_err(crate::Error::ffi)?;
        let mut cluster: rados_t = std::ptr::null_mut();
        unsafe { check_os_error(rados_create(&mut cluster, user.as_ptr()))? };
        let cluster = RadosPtr(NonNull::new(cluster).ok_or_else(|| crate::Error::unexpected("unexpected rados_create failure"))?);
        Ok(Self {
            cluster,
            executor: &NoExecutor,
        })
    }

    /// Create a [`RadosClientBuilder`], connecting as the `admin` user.
    ///
    /// Configuration values will be pulled from the `CEPH_ARGS` environment variable, as well as the first
    /// config file that exists amongst:
    /// - `$CEPH_CONF` (environment variable)
    /// - `/etc/ceph/ceph.conf`
    /// - `~/.ceph/config`
    /// - `./ceph.conf`
    ///
    /// Fails if librados returns an error when creating the cluster handle.
    pub fn try_default() -> crate::Result<Self> {
        Self::new(c"admin")
    }

    /// Specify the [`DynExecutor`] that the [`RadosClient`] will use to spawn async tasks that are
    /// needed internally.
    ///
    /// `executor` can be a marker type like `async_ceph::async_rt::Tokio` that defers to the current Tokio runtime context,
    /// or a reference to the runtime itself.
    pub fn executor(self, executor: &'a dyn DynExecutor) -> RadosClientBuilder<'a> {
        RadosClientBuilder {
            cluster: self.cluster,
            executor,
        }
    }
}

impl<'a, E: ?Sized + 'a> RadosClientBuilder<'a, E> {
    /// Loads a config file into this [`RadosClientBuilder`]'s configuration.
    pub fn load_config_file<P: AsRef<Path>>(self, path: P) -> Result<Self, RadosClientBuilderError> {
        let path = CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;
        unsafe { check_os_error(rados_conf_read_file(self.cluster.as_ptr(), path.as_ptr())).map_err(crate::Error::from)? };
        Ok(self)
    }

    /// Configure the Ceph client based on an environment variable.
    ///
    /// The contents of the environment variable are parsed as if they were
    /// Ceph command line options.
    pub fn load_env(self, var: impl AsRef<OsStr>) -> Result<Self, RadosClientBuilderError> {
        // librados documentation indicates that this function is bugged and uses a static buffer,
        // so it is not safe to call concurrently. However this seems to have been fixed in Ceph 19
        // (squid). Protect it anyway.
        static MUTEX: Mutex<()> = Mutex::new(());
        let _guard = MUTEX.lock().expect("poisoned");

        let var = CString::new(var.as_ref().as_encoded_bytes())?;
        unsafe { check_os_error(rados_conf_parse_env(self.cluster.as_ptr(), var.as_ptr())).map_err(crate::Error::from)? };
        Ok(self)
    }

    /// Configure the Ceph client with command line arguments.
    ///
    /// Any arguments not recognized by librados are returned as a
    /// [`RadosClientBuilderError::UnrecognizedCliArgs`] error. To continue in this scenario,
    /// use [`RadosClientBuilder::load_cli`] instead.
    ///
    /// The arguments can be any common Ceph command line option, including any
    /// configuration parameter prefixed by '--' and replacing spaces with
    /// dashes or underscores. For example, the following options are equivalent:
    /// - `--mon-host 10.0.0.1:6789`
    /// - `--mon_host 10.0.0.1:6789`
    /// - `-m 10.0.0.1:6789`
    pub fn load_cli_strict<I>(self, args: I) -> Result<Self, RadosClientBuilderError>
    where
        I: IntoIterator<Item: AsRef<OsStr>>, {
        let (this, rejected_args) = self.load_cli(args)?;

        if rejected_args.is_empty() {
            Ok(this)
        } else {
            Err(RadosClientBuilderError::UnrecognizedCliArgs(
                rejected_args.into_iter().map(|arg| arg.as_ref().to_owned()).collect(),
            ))
        }
    }

    /// Configure the Ceph client with command line arguments.
    ///
    /// Any arguments not recognized by librados are returned with the builder, so that
    /// they may be passed to something else. To error in this scenario,
    /// use [`RadosClientBuilder::load_cli_strict`] instead.
    ///
    /// The arguments can be any common Ceph command line option, including any
    /// configuration parameter prefixed by '--' and replacing spaces with
    /// dashes or underscores. For example, the following options are equivalent:
    /// - `--mon-host 10.0.0.1:6789`
    /// - `--mon_host 10.0.0.1:6789`
    /// - `-m 10.0.0.1:6789`
    pub fn load_cli<I>(self, args: I) -> Result<(Self, Vec<I::Item>), RadosClientBuilderError>
    where
        I: IntoIterator<Item: AsRef<OsStr>>, {
        let mut args: Vec<_> = args.into_iter().collect();
        let args_cstr = args
            .iter()
            .map(|arg| CString::new(arg.as_ref().as_encoded_bytes()))
            .collect::<Result<Vec<_>, _>>()?;

        let mut argv: Vec<_> = args_cstr.iter().map(|arg| arg.as_ptr()).collect();
        let mut remargv = vec![ptr::null(); argv.len()];

        unsafe {
            check_os_error(rados_conf_parse_argv_remainder(
                self.cluster.as_ptr(),
                argv.len().try_into().map_err(|_| crate::Error::invalid_input("too many arguments"))?,
                argv.as_mut_ptr(),
                remargv.as_mut_ptr(),
            ))
            .map_err(crate::Error::from)?
        };

        let rejected_set: HashSet<_> = remargv.into_iter().take_while(|arg| !arg.is_null()).collect();
        let mut i = 0;
        args.retain(|_| {
            let rejected = rejected_set.contains(&argv[i]);
            i += 1;
            rejected
        });

        Ok((self, args))
    }

    /// Get the value of an individual Ceph client configuration option.
    pub fn get<K: AsRef<str>>(&self, key: K) -> Result<CString, RadosClientBuilderError> {
        let ckey = CString::new(key.as_ref())?;
        let result = unsafe {
            crate::util::get_unknown_size_cstr(
                |buf, size| rados_sys::rados_conf_get(self.cluster.as_ptr(), ckey.as_ptr(), buf, size),
                32,
            )
        };
        match result {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(RadosClientBuilderError::InvalidOption(key.as_ref().into())),
            Err(e) => Err(RadosClientBuilderError::Rados(e.into())),
        }
    }

    /// Set individual Ceph client configuration options.
    ///
    /// Commonly set options include:
    ///  - `mon_host`
    ///  - `auth_supported`
    ///  - `key`, `keyfile`, or `keyring` when using cephx
    ///  - `log_file`, `log_to_stderr`, `err_to_stderr`, and `log_to_syslog`
    ///  - `debug_rados`, `debug_objecter`, `debug_monc`, `debug_auth`, or `debug_ms`
    ///
    /// Consult the ceph code or documentation for an extensive list of configuration options.
    pub fn set<K: AsRef<str>, V: AsRef<OsStr>>(self, key: K, value: V) -> Result<Self, RadosClientBuilderError> {
        let ckey = CString::new(key.as_ref())?;
        let value = CString::new(value.as_ref().as_encoded_bytes())?;

        let result = unsafe { rados_conf_set(self.cluster.as_ptr(), ckey.as_ptr(), value.as_ptr()) };
        if result == -libc::ENOENT {
            return Err(RadosClientBuilderError::InvalidOption(key.as_ref().into()));
        }

        check_os_error(result).map_err(crate::Error::from)?;
        Ok(self)
    }
}

impl<'a> RadosClientBuilder<'a> {
    /// Try to connect to a Ceph cluster according to the current configuration.
    ///
    /// By default, when this [`RadosClientBuilder`] is created configuration values will be pulled from
    /// the `CEPH_ARGS` environment variable, as well as the first config file that exists amongst:
    /// - `$CEPH_CONF` (environment variable)
    /// - `/etc/ceph/ceph.conf`
    /// - `~/.ceph/config`
    /// - `./ceph.conf`
    ///
    /// A different config file can be specified using [`RadosClientBuilder::load_config_file`], and
    /// individual key-value configuration options can be set using the [`set`](Self::set) method.
    #[tracing::instrument("RadosClientBuilder::connect", level = "debug")]
    pub async fn connect(self) -> crate::Result<RadosClient<'a>> {
        let handle = RadosClientHandle::new(self.cluster);
        let (connect_result, handle) = self
            .executor
            .spawn_blocking(move || unsafe { (check_os_error(rados_connect(handle.as_ptr())), handle) })
            .await
            .expect("failed to await join handle task");

        connect_result.map_err(crate::Error::from)?;
        Ok(RadosClient {
            handle,
            executor: self.executor,
        })
    }
}

impl Default for RadosClientBuilder<'_, NoExecutor> {
    /// Create a [`RadosClientBuilder`], connecting as the `admin` user.
    ///
    /// The to-be-created [`RadosClient`] will use the provided [`DynExecutor`] to spawn async tasks
    /// that are needed internally. This can be a marker type like `async_ceph::async_rt::Tokio` that defers
    /// to the current Tokio runtime context, or a reference to the runtime itself.
    ///
    /// Configuration values will be pulled from the `CEPH_ARGS` environment variable, as well as the first
    /// config file that exists amongst:
    /// - `$CEPH_CONF` (environment variable)
    /// - `/etc/ceph/ceph.conf`
    /// - `~/.ceph/config`
    /// - `./ceph.conf`
    ///
    /// # Panics
    ///
    /// if librados returns an error when creating the cluster handle.
    fn default() -> Self {
        Self::try_default().unwrap()
    }
}

#[cfg(all(test, not(loom), not(miri)))]
mod test {
    use std::error::Error;

    use crate::rados::{RadosClientBuilder, RadosClientBuilderError};

    #[test]
    fn invalid_option_str() -> Result<(), Box<dyn Error>> {
        let rados = RadosClientBuilder::default();
        let res = rados.set("key", "va\0lue");

        assert!(matches!(res, Err(RadosClientBuilderError::InvalidString(_))));

        Ok(())
    }

    #[test]
    fn invalid_opts() -> Result<(), Box<dyn Error>> {
        let rados = RadosClientBuilder::default();
        let res = rados.set("invalid_config_key", "value");

        assert!(matches!(res, Err(RadosClientBuilderError::InvalidOption(k)) if k == "invalid_config_key"));

        Ok(())
    }

    #[test]
    fn set_env() -> Result<(), Box<dyn Error>> {
        let builder = RadosClientBuilder::default().set("mon_host", "1.2.3.4")?;
        assert_eq!(builder.get("mon_host")?, c"1.2.3.4");

        Ok(())
    }

    #[test]
    fn load_env() -> Result<(), Box<dyn Error>> {
        unsafe { std::env::set_var("FOO", "--mon-host 1.2.3.4 --not-a-ceph-arg 1234") };

        let builder = RadosClientBuilder::default().load_env("FOO")?;
        assert_eq!(builder.get("mon_host")?, c"1.2.3.4");

        Ok(())
    }

    #[test]
    fn load_cli() -> Result<(), Box<dyn Error>> {
        let args = "--mon-host 1.2.3.4 --not-a-ceph-arg 1234";
        let (builder, rejected) = RadosClientBuilder::default().load_cli(args.split(' '))?;

        assert_eq!(builder.get("mon_host")?, c"1.2.3.4");
        assert_eq!(rejected, ["--not-a-ceph-arg", "1234"]);

        Ok(())
    }

    #[test]
    fn load_cli_strict() {
        let args = "--mon-host 1.2.3.4 --not-a-ceph-arg 1234";
        let res = RadosClientBuilder::default().load_cli_strict(args.split(' '));

        assert!(matches!(res,
            Err(RadosClientBuilderError::UnrecognizedCliArgs(args))
            if args == ["--not-a-ceph-arg", "1234"]));
    }
}
