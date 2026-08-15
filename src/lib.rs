// Copyright © 2024-26 The Johns Hopkins Applied Physics Laboratory LLC.
//
// This program is free software: you can redistribute it and/or
// modify it under the terms of the GNU Affero General Public License,
// version 3, as published by the Free Software Foundation.  If you
// would like to purchase a commercial license for this software, please
// contact APL’s Tech Transfer at 240-592-0817 or
// techtransfer@jhuapl.edu.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public
// License along with this program.  If not, see
// <https://www.gnu.org/licenses/>.

#![allow(clippy::redundant_field_names)]

//! Common support for running Constellation components as standalone
//! executables.
//!
//! This package provides the [Standalone] trait, which can be
//! implemented by Constellation components in order to allow them to
//! be easily turned into standalone executables.  This trait provides
//! a common [main](Standalone::main) function which can be called
//! from a one-line top-level `main`.  It handles reading in
//! configurations, setting up logging, and other tasks in a common
//! manner.

use std::env::VarError;
use std::ffi::CString;
use std::fs::File;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering;

use clap::command;
use clap::Arg;
use clap::ArgAction;
use clap::ArgMatches;
use clap::Command;
use constellation_common::shutdown::ShutdownFlag;
use constellation_common::version::FullVersion;
use daemonize::Daemonize;
use libc::c_int;
use libc::sighandler_t;
use libc::signal;
use libc::strerror;
use libc::SIGHUP;
use libc::SIGINT;
use libc::SIGTERM;
use log::debug;
use log::error;
use log::info;
use log::trace;
use log::LevelFilter;
use log4rs::append::console::ConsoleAppender;
use log4rs::config::load_config_file;
use log4rs::config::Appender;
use log4rs::config::Deserializers;
use log4rs::config::Root;
use log4rs::Config;
use log4rs::Handle;
use serde::Deserialize;

const DEFAULT_CONFDIR_ENV_NAME: &str = "CONSTELLATION_CONFDIR";
const DEFAULT_RUNDIR_ENV_NAME: &str = "CONSTELLATION_RUNDIR";
const DEFAULT_LOGLVL_ENV_NAME: &str = "CONSTELLATION_LOGLVL";
const DEFAULT_USER_ENV_NAME: &str = "CONSTELLATION_USER";
const DEFAULT_GROUP_ENV_NAME: &str = "CONSTELLATION_GROUP";
const DEFAULT_RUNDIR: &str = "/var/run/constellation";
const DEFAULT_SYSTEM_CONFDIR: &str = "/usr/local/etc/constellation";
const DEFAULT_HOME_CONFSUBDIR: &str = ".config/constellation";
const DEFAULT_USER: &str = "constellation";
const DEFAULT_GROUP: &str = "constellation";

/// Base trait for standalone components.
///
/// This trait defines a common interface used by [StandaloneService]
/// and [StandaloneApp] to manage the following tasks:
///
/// * Identifying the location of configuration files, and reading them in.
///
/// * Setting up logging.
///
/// * Setting up signal handlers to trigger shutdown.
///
/// * Setting up and running the component.
///
/// * Cleanly shutting down.
pub trait Standalone: Sized {
    const LOGLVL_ENV: &str = DEFAULT_LOGLVL_ENV_NAME;
    const CONFIG_DIR_ENV: &str = DEFAULT_CONFDIR_ENV_NAME;

    /// Location of the system-wide configuration directory.
    ///
    /// Defaults to `/usr/local/etc/constellation`.
    const SYSTEM_CONFIG_DIR: &str = DEFAULT_SYSTEM_CONFDIR;

    /// Subdirectory under which home directory configurations are stored.
    ///
    /// Defaults to `.config/constellation`.
    const HOME_CONFIG_SUBDIR: &str = DEFAULT_HOME_CONFSUBDIR;

    /// Name of the standalone component.
    const NAME: &str;

    /// Possible names of component configuration files.
    ///
    /// These are given in order of preference.
    const CONFIG_FILES: &[&str];

    /// Possible names of logging configuration files.
    ///
    /// These are given in order of preference.
    ///
    /// Defaults to a single entry, `constellation-log.yaml`.
    const LOG_CONFIG_FILES: &[&str] = &["constellation-log.yaml"];

    /// Version string for this application.
    const VERSION: FullVersion;

    /// Type of configuration objects.
    type Config: for<'de> Deserialize<'de>;

    /// Type of cleanup objects from [run](Standalone::create).
    type CreateCleanup;

    /// Add command-line arguments to be parsed.
    fn cmdargs(cmd: Command) -> Command {
        cmd
    }

    /// Create an instance of the component from a configuration.
    fn create(
        args: ArgMatches,
        config: Self::Config
    ) -> Result<(Self, Self::CreateCleanup), Self::CreateCleanup>;
}

/// Trait for standalone services.
///
/// This provides a [main](Standalone::main) function that can be
/// called directly from the top-level `main`.  It also manages the
/// following tasks:
///
/// * Identifying the location of configuration files, and reading them in.
///
/// * Setting up logging.
///
/// * Setting up signal handlers to trigger shutdown.
///
/// * Setting up and running the component as a service.
///
/// * Cleanly shutting down.
///
/// # Usage
///
/// In order to use the facilities provided by this trait, a top-level
/// Constellation component should implement it, which will provide
/// the necessary definitions for the configuration types, how to
/// initialize the component, how to run it, and how to shut it down.
///
/// The `main` function should then simply call [Standalone::main].
pub trait StandaloneService: Standalone {
    const RUN_DIR_ENV: &str = DEFAULT_RUNDIR_ENV_NAME;
    const USER_ENV: &str = DEFAULT_USER_ENV_NAME;
    const GROUP_ENV: &str = DEFAULT_GROUP_ENV_NAME;
    const DEFAULT_RUN_DIR: &str = DEFAULT_RUNDIR;
    const DEFAULT_USER: Option<&str> = Some(DEFAULT_USER);
    const DEFAULT_GROUP: Option<&str> = Some(DEFAULT_GROUP);
    const DEFAULT_PIDFILE: &str;

    /// Type of cleanup objects from [run](Standalone::run).
    type RunCleanup;

    /// Type of cleanup objects produced from errors in [run](Standalone::run).
    type RunErrorCleanup;

    /// Entrypoint for the component.
    fn run(
        self,
        rundir: PathBuf
    ) -> Result<Self::RunCleanup, Self::RunErrorCleanup>;

    /// Shut down the component and clean up any resources.
    ///
    /// The two cleanup objects `create` and `run` are the same that
    /// are returned by [create](Standalone::create) and
    /// [run](Standalone::run).
    fn shutdown(
        create: Self::CreateCleanup,
        run: Option<Self::RunCleanup>
    );

    /// Shut down the component and clean up any resources in the
    /// event of an error.
    ///
    /// The two cleanup objects `create` and `run` are the same that
    /// are returned by [create](Standalone::create) and
    /// [run](Standalone::run) (if it returned an error).
    fn shutdown_err(
        create: Self::CreateCleanup,
        run: Self::RunErrorCleanup
    );

    /// A complete `main` function implementation for a standalone
    /// component.
    ///
    /// This can be called from the executable `main` as its only
    /// content.
    fn main() {
        let cmd = service_cmdargs::<Self>();
        let cmd = Self::cmdargs(cmd);
        let mut arg_matches = cmd.get_matches();
        let Args { confdir, loglvl } = match get_args::<Self>(&mut arg_matches)
        {
            Ok(args) => args,
            Err(err) => {
                eprintln!("{}", err);

                std::process::exit(1);
            }
        };
        let ServiceArgs {
            pidfile,
            rundir,
            daemon,
            user,
            group
        } = match get_service_args::<Self>(&mut arg_matches) {
            Ok(args) => args,
            Err(err) => {
                eprintln!("{}", err);

                std::process::exit(1);
            }
        };

        // First set up the bootstrap logger.
        let handle = bootstrap_log_setup(loglvl);

        // Get the configuration directories.
        let dirs = config_dirs::<Self>(confdir);

        // Set up the permanent logger.
        log_setup::<Self>(&dirs, &handle);

        if let Some(config) = load_config::<Self>(&dirs) {
            if daemon {
                let daemon =
                    Daemonize::new().pid_file(&pidfile).chown_pid_file(true);

                let daemon = if let Some(user) = user {
                    daemon.user(user.as_str())
                } else {
                    daemon
                };

                let daemon = if let Some(group) = group {
                    daemon.group(group.as_str())
                } else {
                    daemon
                };

                if let Err(err) = daemon.start() {
                    error!("failed to detach from terminal: {}", err);
                }
            }

            match Self::create(arg_matches, config) {
                Ok((app, create_cleanup)) => {
                    // Register signal handlers and start the service.
                    if shutdown_signals(true).is_ok() {
                        match app.run(rundir) {
                            Ok(run_cleanup) => {
                                Self::shutdown(
                                    create_cleanup,
                                    Some(run_cleanup)
                                );

                                info!(target: "standalone",
                                      "{} shutdown successful",
                                      Self::NAME);
                            }
                            Err(err) => {
                                Self::shutdown_err(create_cleanup, err);

                                std::process::exit(1);
                            }
                        }
                    } else {
                        Self::shutdown(create_cleanup, None);
                    }
                }
                Err(cleanup) => {
                    debug!(target: "standalone",
                           "cleaning up after create error");

                    Self::shutdown(cleanup, None);

                    info!(target: "standalone",
                          "{} cleaned up after error",
                          Self::NAME);

                    std::process::exit(1);
                }
            }

            // Delete the pidfile if we're a daemon.
            if daemon && let Err(err) = std::fs::remove_file(pidfile) {
                error!("failed to remove pid file: {}", err);

                std::process::exit(1);
            }
        } else {
            error!("could not obtain valid configuration");

            std::process::exit(1);
        }
    }
}
/// Trait for standalone applications.
///
/// This provides a [main](Standalone::main) function that can be
/// called directly from the top-level `main`.  It also manages the
/// following tasks:
///
/// * Identifying the location of configuration files, and reading them in.
///
/// * Setting up logging.
///
/// * Setting up signal handlers to trigger shutdown.
///
/// * Setting up and running the application.
///
/// * Cleanly shutting down.
///
/// # Usage
///
/// In order to use the facilities provided by this trait, a top-level
/// Constellation component should implement it, which will provide
/// the necessary definitions for the configuration types, how to
/// initialize the component, how to run it, and how to shut it down.
///
/// The `main` function should then simply call [Standalone::main].
pub trait StandaloneApp: Standalone {
    /// Type of cleanup objects produced from errors in [run](Standalone::run).
    type RunErrorCleanup;

    /// Entrypoint for the component.
    fn run(self) -> Result<(), Self::RunErrorCleanup>;

    /// Shut down the component and clean up any resources.
    ///
    /// The two cleanup objects `create` and `run` are the same that
    /// are returned by [create](Standalone::create) and
    /// [run](Standalone::run).
    fn cleanup(create: Self::CreateCleanup);

    /// Shut down the component and clean up any resources in the
    /// event of an error.
    ///
    /// The two cleanup objects `create` and `run` are the same that
    /// are returned by [create](Standalone::create) and
    /// [run](Standalone::run) (if it returned an error).
    fn cleanup_err(
        create: Self::CreateCleanup,
        run: Self::RunErrorCleanup
    );

    /// A complete `main` function implementation for a standalone
    /// component.
    ///
    /// This can be called from the executable `main` as its only
    /// content.
    fn main() {
        let cmd = cmdargs_setup::<Self>();
        let cmd = Self::cmdargs(cmd);
        let mut arg_matches = cmd.get_matches();
        let Args { confdir, loglvl } = match get_args::<Self>(&mut arg_matches)
        {
            Ok(args) => args,
            Err(err) => {
                eprintln!("{}", err);

                std::process::exit(1);
            }
        };

        // First set up the bootstrap logger.
        let handle = bootstrap_log_setup(loglvl);

        // Get the configuration directories.
        let dirs = config_dirs::<Self>(confdir);

        // Set up the permanent logger.
        log_setup::<Self>(&dirs, &handle);

        if let Some(config) = load_config::<Self>(&dirs) {
            match Self::create(arg_matches, config) {
                Ok((app, create_cleanup)) => {
                    // Register signal handlers and start the service.
                    if shutdown_signals(true).is_ok() {
                        match app.run() {
                            Ok(()) => {
                                Self::cleanup(create_cleanup);
                            }
                            Err(err) => {
                                Self::cleanup_err(create_cleanup, err);

                                std::process::exit(1);
                            }
                        }
                    } else {
                        Self::cleanup(create_cleanup);
                    }
                }
                Err(cleanup) => {
                    debug!(target: "standalone",
                           "cleaning up after create error");

                    Self::cleanup(cleanup);

                    info!(target: "standalone",
                          "{} cleaned up after error",
                          Self::NAME);

                    std::process::exit(1);
                }
            }
        } else {
            error!(target: "load-config",
                   "could not obtain valid configuration");

            std::process::exit(1);
        }
    }
}

pub enum MainError {
    LoadConfError
}

struct Args {
    /// Initial logging level.
    loglvl: LevelFilter,
    /// Configuration directory.
    confdir: Option<PathBuf>
}

struct ServiceArgs {
    /// Path to pidfile.
    pidfile: PathBuf,
    /// Run directory.
    rundir: PathBuf,
    user: Option<String>,
    group: Option<String>,
    daemon: bool
}

struct RegisterSignalsError;

struct LockFreeList {
    flag: ShutdownFlag,
    next: *mut LockFreeList
}

static SHUTDOWN_FLAGS: AtomicPtr<LockFreeList> =
    AtomicPtr::new(std::ptr::null_mut());

/// Register a shutdown flag, which will be raised when a termination
/// signal is received.
pub fn register_shutdown(flag: ShutdownFlag) {
    let new = Box::into_raw(Box::new(LockFreeList {
        flag: flag,
        next: std::ptr::null_mut()
    }));

    while {
        let curr = SHUTDOWN_FLAGS.load(Ordering::Relaxed);

        unsafe {
            (*new).next = curr;
        }

        SHUTDOWN_FLAGS
            .compare_exchange_weak(
                curr,
                new,
                Ordering::AcqRel,
                Ordering::Relaxed
            )
            .is_err()
    } {}
}

fn shutdown_signals(sighup: bool) -> Result<(), RegisterSignalsError> {
    static mut SHUTDOWN_ON_INT: bool = false;

    unsafe extern "C" fn handler(sig: c_int) {
        if sig == SIGINT {
            unsafe {
                if SHUTDOWN_ON_INT {
                    exit(1);
                } else {
                    SHUTDOWN_ON_INT = true
                }
            }
        }

        let mut curr = SHUTDOWN_FLAGS.load(Ordering::Acquire);

        while !curr.is_null() {
            unsafe {
                if let Err(err) = (*curr).flag.set() {
                    error!(target: "signal-handler",
                           "error sending shutdown notification: {}",
                           err);
                }

                curr = (*curr).next;
            }
        }
    }

    match unsafe { signal(SIGTERM, handler as *const () as sighandler_t) } {
        0 => Ok(()),
        err => {
            report_signal_error(err);

            Err(RegisterSignalsError)
        }
    }?;

    match unsafe { signal(SIGINT, handler as *const () as sighandler_t) } {
        0 => Ok(()),
        err => {
            report_signal_error(err);

            Err(RegisterSignalsError)
        }
    }?;

    if sighup {
        match unsafe { signal(SIGHUP, handler as *const () as sighandler_t) } {
            0 => Ok(()),
            err => {
                report_signal_error(err);

                Err(RegisterSignalsError)
            }
        }?;
    }

    Ok(())
}

fn report_signal_error(err: usize) {
    let cstr = unsafe {
        let raw = strerror(err as i32);

        if raw.is_null() {
            CString::from_vec_unchecked(vec![0])
        } else {
            CString::from_raw(raw)
        }
    };

    match cstr.into_string() {
        Ok(str) => {
            error!(target: "standalone",
                   "error registering signal handler: {}",
                   str);
        }
        Err(err) => {
            error!(target: "standalone",
                   "error converting string: {}",
                   err)
        }
    }
}

fn bootstrap_log_setup(loglvl: LevelFilter) -> Handle {
    // Set up an initial logger.  This will be used to report any
    // errors loading the configuration.
    let console = ConsoleAppender::builder().build();
    let log_config = match Config::builder()
        .appender(Appender::builder().build("console", Box::new(console)))
        .build(Root::builder().appender("console").build(loglvl))
    {
        Ok(log_config) => log_config,
        Err(err) => {
            panic!("Error initializing bootstrap logger: {}", err);
        }
    };

    let handle = match log4rs::init_config(log_config) {
        Ok(handle) => handle,
        Err(err) => {
            panic!("Error initializing bootstrap logger: {}", err);
        }
    };

    debug!(target: "log-setup",
           "bootstrap logger initialized");

    handle
}

fn get_service_args<S: StandaloneService>(
    args: &mut ArgMatches
) -> Result<ServiceArgs, String> {
    let component_confdir_env_name =
        format!("CONSTELLATION_{}_RUNDIR", S::NAME.to_uppercase());
    let component_rundir_env = match std::env::var(&component_confdir_env_name)
    {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_confdir_env_name
        ))
    }?;
    let rundir_env = match std::env::var(S::RUN_DIR_ENV) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            S::RUN_DIR_ENV
        ))
    }?;
    let rundir = args
        .remove_one("rundir")
        .or(component_rundir_env)
        .or(rundir_env)
        .unwrap_or(String::from(S::DEFAULT_RUN_DIR));
    let component_pidfile_env_name =
        format!("CONSTELLATION_{}_PIDFILE", S::NAME.to_uppercase());
    let component_pidfile_env = match std::env::var(&component_pidfile_env_name)
    {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_pidfile_env_name
        ))
    }?;
    let default_pidfile = format!("{}/{}", rundir, S::DEFAULT_PIDFILE);
    let rundir = PathBuf::from(rundir);
    let pidfile = args
        .remove_one("pidfile")
        .or(component_pidfile_env)
        .unwrap_or(default_pidfile);
    let pidfile = PathBuf::from(pidfile);
    let daemon = args.remove_one("daemon").unwrap_or(false);
    let component_user_env_name =
        format!("CONSTELLATION_{}_USER", S::NAME.to_uppercase());
    let component_user_env = match std::env::var(&component_user_env_name) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_confdir_env_name
        ))
    }?;
    let user_env = match std::env::var(S::USER_ENV) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            S::USER_ENV
        ))
    }?;
    let user = args
        .remove_one("user")
        .or(component_user_env)
        .or(user_env)
        .or_else(|| S::DEFAULT_USER.map(String::from));
    let component_group_env_name =
        format!("CONSTELLATION_{}_GROUP", S::NAME.to_uppercase());
    let component_group_env = match std::env::var(&component_group_env_name) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_confdir_env_name
        ))
    }?;
    let group_env = match std::env::var(S::GROUP_ENV) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            S::GROUP_ENV
        ))
    }?;
    let group = args
        .remove_one("group")
        .or(component_group_env)
        .or(group_env)
        .or_else(|| S::DEFAULT_GROUP.map(String::from));

    Ok(ServiceArgs {
        pidfile: pidfile,
        rundir: rundir,
        daemon: daemon,
        user: user,
        group: group
    })
}

fn get_args<S: Standalone>(args: &mut ArgMatches) -> Result<Args, String> {
    let component_confdir_env_name =
        format!("CONSTELLATION_{}_CONFDIR", S::NAME.to_uppercase());
    let component_confdir_env = match std::env::var(&component_confdir_env_name)
    {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_confdir_env_name
        ))
    }?;
    let confdir_env = match std::env::var(S::CONFIG_DIR_ENV) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            S::CONFIG_DIR_ENV
        ))
    }?;
    let confdir = args
        .remove_one("confdir")
        .or(component_confdir_env)
        .or(confdir_env)
        .map(PathBuf::from);
    let verbose = args.get_count("verbosity");
    let verbose_lvl: LevelFilter = match verbose {
        0 => Ok(LevelFilter::Warn),
        1 => Ok(LevelFilter::Info),
        2 => Ok(LevelFilter::Debug),
        3 => Ok(LevelFilter::Trace),
        _ => Err("More than three verbose flags is redundant")
    }?;
    let component_loglvl_env_name =
        format!("CONSTELLATION_{}_LOGLVL", S::NAME.to_uppercase());
    let component_loglvl_env = match std::env::var(&component_loglvl_env_name) {
        Ok(val) => {
            if verbose == 0 {
                Ok(Some(val))
            } else {
                Err(format!(
                    concat!(
                        "Cannot use verbose flag when setting ",
                        "log level through environment variable {}"
                    ),
                    component_loglvl_env_name
                ))
            }
        }
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_loglvl_env_name
        ))
    }?;
    let loglvl_env = match std::env::var(S::LOGLVL_ENV) {
        Ok(val) => {
            if verbose == 0 {
                Ok(Some(val))
            } else {
                Err(format!(
                    concat!(
                        "Cannot use verbose flag when setting ",
                        "log level through environment variable {}"
                    ),
                    S::LOGLVL_ENV
                ))
            }
        }
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            S::LOGLVL_ENV
        ))
    }?;
    let loglvl = match args.remove_one("loglvl") {
        Some(_) if verbose != 0 => Err(concat!(
            "Cannot use verbose flag when setting ",
            "log level through command line"
        )
        .to_string()),
        loglvl_arg => match loglvl_arg.or(component_loglvl_env).or(loglvl_env) {
            Some(loglvl) => LevelFilter::from_str(&loglvl)
                .map(Some)
                .map_err(|err| format!("error parsing loglvl: {}", err)),
            None => Ok(None)
        }
    }?
    .unwrap_or(verbose_lvl);

    Ok(Args {
        loglvl: loglvl,
        confdir: confdir
    })
}

fn service_cmdargs<S: Standalone>() -> Command {
    cmdargs_setup::<S>()
        .arg(
            Arg::new("rundir")
                .short('r')
                .long("rundir")
                .help("Run directory")
        )
        .arg(
            Arg::new("daemon")
                .short('d')
                .long("daemon")
                .action(ArgAction::SetTrue)
                .help("Run as a daemon")
        )
        .arg(
            Arg::new("pidfile")
                .long("pidfile")
                .requires("daemon")
                .help("Location of PID file")
        )
        .arg(
            Arg::new("user")
                .short('u')
                .long("user")
                .requires("daemon")
                .help("User to run as")
        )
        .arg(
            Arg::new("group")
                .short('g')
                .long("group")
                .requires("daemon")
                .help("Group to run as")
        )
}

/// Set up command-line argument parser.
fn cmdargs_setup<S: Standalone>() -> Command {
    command!()
        .version(S::VERSION.to_string())
        .arg(
            Arg::new("confdir")
                .short('c')
                .long("confdir")
                .help("Location of configuration files")
        )
        .arg(
            Arg::new("loglvl")
                .short('l')
                .long("loglvl")
                .value_parser(["error", "warn", "info", "debug", "trace"])
                .help("Set logging level")
        )
        .arg(
            Arg::new("verbosity")
                .short('v')
                .long("verbose")
                .help("Increase logging verbosity")
                .action(ArgAction::Count)
        )
}

/// Get the set of configuration directories to search for
/// configuration files.
fn config_dirs<S: Standalone>(arg_confdir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);

    // Home configuration directory first.
    if let Ok(path) = std::env::var("HOME") {
        let mut pathbuf =
            PathBuf::with_capacity(path.len() + S::HOME_CONFIG_SUBDIR.len());

        pathbuf.push(path);
        pathbuf.push(S::HOME_CONFIG_SUBDIR);
        pathbuf.shrink_to_fit();

        debug!(target: "standalone",
               "adding configuration directory {}",
               pathbuf.to_string_lossy());

        out.push(pathbuf);
    }

    // Check for configuration directory overrides.
    if let Some(path) = arg_confdir {
        // Component-specific configuration directory.
        debug!(target: "standalone",
               "adding configuration directory {}",
               path.to_string_lossy());

        out.push(path)
    } else if let Ok(path) = std::env::var("CONSTELLATION_CONF_DIR") {
        // General configuration directory.
        debug!(target: "standalone",
               "adding configuration directory {}",
               path);

        out.push(PathBuf::from(path))
    } else {
        debug!(target: "standalone",
               "adding configuration directory {}",
               S::SYSTEM_CONFIG_DIR);

        out.push(PathBuf::from(S::SYSTEM_CONFIG_DIR))
    }

    out.shrink_to_fit();

    out
}

/// Set up the permanent logger.
fn log_setup<S: Standalone>(
    dirs: &[PathBuf],
    handle: &Handle
) {
    debug!(target: "log-setup",
           "loading permanent logging configuration");

    // Use configuration to set up the permanent logger.
    for file in S::LOG_CONFIG_FILES {
        debug!(target: "log-setup",
               "looking for logging configuration file {}",
               file);

        for dir in dirs.iter() {
            let path = dir.join(file);

            trace!(target: "log-setup",
                   "trying path {}",
                   path.to_string_lossy());

            if path.is_file() {
                debug!(target: "log-setup",
                       "loading log config file {}",
                       path.to_string_lossy());

                match load_config_file(path.clone(), Deserializers::new()) {
                    Ok(config) => {
                        debug!(target: "log-setup",
                               "found valid logging configuration");

                        handle.set_config(config);

                        debug!(target: "log-setup",
                               "permanent logger initialized");

                        return;
                    }
                    Err(err) => {
                        error!(target: "log-setup",
                               "error loading config file: {}", err);
                    }
                }
            } else {
                trace!(target: "log-setup",
                       "file {} not found",
                       path.to_string_lossy());
            }
        }
    }

    debug!(target: "log-setup",
           "keeping bootstrap logger");
}

/// Load a configuration file from a set of paths, and a set of
/// possible names.
fn load_config<S: Standalone>(dirs: &[PathBuf]) -> Option<S::Config> {
    let names = S::CONFIG_FILES.iter().copied();

    debug!(target: "load-config",
           "loading main configuration");

    for file in names {
        debug!(target: "load-config",
               "looking for main configuration file {}",
               file);

        for dir in dirs.iter() {
            let path = dir.join(file);

            trace!(target: "load-config",
                   "trying path {}",
                   path.to_string_lossy());

            if path.is_file() {
                debug!(target: "loag-config",
                       "loading config file {}",
                       path.to_string_lossy());

                match File::open(path.clone()) {
                    Ok(file) => match serde_yaml::from_reader(file) {
                        Ok(yaml) => {
                            trace!(target: "load-config",
                                   "found valid configuration");

                            return Some(yaml);
                        }
                        Err(err) => {
                            error!(target: "load-config",
                                   "error parsing configuration at {}: {}",
                                   path.to_string_lossy(), err);
                        }
                    },
                    Err(err) => {
                        error!(target: "load-config",
                               "error loading file: {}",
                               err)
                    }
                };
            } else {
                trace!(target: "load-config",
                       "file {} not found",
                       path.to_string_lossy());
            }
        }
    }

    None
}
