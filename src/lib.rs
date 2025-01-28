// Copyright © 2024 The Johns Hopkins Applied Physics Laboratory LLC.
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
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;

use clap::command;
use clap::Arg;
use clap::ArgAction;
use clap::ArgMatches;
use clap::Command;
use constellation_common::sync::Notify;
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

const CONFDIR_ENV_NAME: &str = "CONSTELLATION_CONFDIR";
const LOGLVL_ENV_NAME: &str = "CONSTELLATION_LOGLVL";

/// Trait to be implemented by standalone components.
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
/// * Setting up and running the component.
///
/// * Cleanly shutting down.
///
/// # Usage
///
/// In order to use the facilities provided by this trait, a top-level
/// Constellation component should implement it, which provides the
/// necessary definitions for the configuration types, how to
/// initialize the component, how to run it, and how to shut it down.
///
/// The `main` function should then simply call [Standalone::main].
pub trait Standalone: Sized {
    const CONFIG_DIR_ENV: &'static str = "CONSTELLATION_CONF_DIR";

    /// Location of the system-wide configuration directory.
    ///
    /// Defaults to `/usr/local/etc/constellation`.
    const SYSTEM_CONFIG_DIR: &'static str = "/usr/local/etc/constellation/";

    /// Subdirectory under which home directory configurations are stored.
    ///
    /// Defaults to `.config/constellation`.
    const HOME_CONFIG_SUBDIR: &'static str = ".config/constellation/";

    /// Name of the standalone component.
    const COMPONENT_NAME: &'static str;

    /// Possible names of component configuration files.
    ///
    /// These are given in order of preference.
    const CONFIG_FILES: &'static [&'static str];

    /// Possible names of logging configuration files.
    ///
    /// These are given in order of preference.
    ///
    /// Defaults to a single entry, `constellation-log.yaml`.
    const LOG_CONFIG_FILES: &'static [&'static str] =
        &["constellation-log.yaml"];

    /// Version string for this application.
    const VERSION: &str;

    /// Type of configuration objects.
    type Config: for<'de> Deserialize<'de>;

    /// Type of cleanup objects from [run](Standalone::run).
    type RunCleanup;

    /// Type of cleanup objects produced from errors in [run](Standalone::run).
    type RunErrorCleanup;

    /// Type of cleanup objects from [run](Standalone::create).
    type CreateCleanup;

    /// Add command-line arguments to be parsed.
    fn cmdargs(_cmd: &mut Command) {}

    /// Create an instance of the component from a configuration.
    fn create(
        args: ArgMatches,
        config: Self::Config
    ) -> Result<(Self, Self::CreateCleanup), Self::CreateCleanup>;

    /// Entrypoint for the component.
    fn run(self) -> Result<Self::RunCleanup, Self::RunErrorCleanup>;

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
        let mut arg_matches = cmdargs_setup::<Self>().get_matches();
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
                    // Register signal handlers.

                    unsafe {
                        SHUTDOWN_NOTIFY.write(Notify::new());
                    }

                    match unsafe { signal(SIGTERM, handler as sighandler_t) } {
                        0 => {}
                        err => {
                            report_signal_error(err);
                            Self::shutdown(create_cleanup, None);

                            return;
                        }
                    };

                    match unsafe { signal(SIGINT, handler as sighandler_t) } {
                        0 => {}
                        err => {
                            report_signal_error(err);
                            Self::shutdown(create_cleanup, None);

                            return;
                        }
                    };

                    match unsafe { signal(SIGHUP, handler as sighandler_t) } {
                        0 => {}
                        err => {
                            report_signal_error(err);
                            Self::shutdown(create_cleanup, None);

                            return;
                        }
                    };

                    match app.run() {
                        Ok(run_cleanup) => {
                            if unsafe {
                                SHUTDOWN_NOTIFY
                                    .assume_init_mut()
                                    .wait_no_reset()
                                    .is_err()
                            } {
                                error!(target: "standalone",
                                       "bad condition variable")
                            }

                            Self::shutdown(create_cleanup, Some(run_cleanup));

                            info!(target: "standalone",
                                  "{} shutdown successful",
                                  Self::COMPONENT_NAME);
                        }
                        Err(err) => {
                            Self::shutdown_err(create_cleanup, err);
                        }
                    }
                }
                Err(cleanup) => {
                    debug!(target: "standalone",
                           "cleaning up after create error");

                    Self::shutdown(cleanup, None);

                    info!(target: "standalone",
                          "{} cleaned up after error",
                          Self::COMPONENT_NAME);
                }
            }
        } else {
            error!(target: "load-config",
                   "could not obtain valid configuration");
        }
    }
}

struct Args {
    /// Initial logging level.
    loglvl: LevelFilter,
    /// Configuration directory.
    confdir: Option<PathBuf>
}

static mut SHUTDOWN_NOTIFY: MaybeUninit<Notify> = MaybeUninit::uninit();
static mut SHUTDOWN_ON_INT: bool = false;

unsafe extern "C" fn handler(sig: c_int) {
    if sig == SIGINT {
        if SHUTDOWN_ON_INT {
            exit(1);
        } else {
            SHUTDOWN_ON_INT = true
        }
    }

    if let Err(err) = SHUTDOWN_NOTIFY.assume_init_mut().notify() {
        error!(target: "signal-handler",
               "error sending shutdown notification: {}",
               err);
    }
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

fn get_args<S: Standalone>(args: &mut ArgMatches) -> Result<Args, String> {
    let component_confdir_env_name =
        format!("CONSTELLATION_{}_CONFDIR", S::COMPONENT_NAME.to_uppercase());
    let component_confdir_env = match std::env::var(&component_confdir_env_name)
    {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            component_confdir_env_name
        ))
    }?;
    let confdir_env = match std::env::var(CONFDIR_ENV_NAME) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            CONFDIR_ENV_NAME
        ))
    }?;
    let confdir = args
        .remove_one("confdir")
        .or(component_confdir_env)
        .or(confdir_env)
        .map(PathBuf::from);
    // let component_pidfile_env_name = format!(
    // "CONSTELLATION_{}_PIDFILE",
    // S::COMPONENT_NAME.to_uppercase()
    // );
    // let component_pidfile_env =
    // match std::env::var(&component_pidfile_env_name) {
    // Ok(val) => Ok(Some(val)),
    // Err(VarError::NotPresent) => Ok(None),
    // Err(VarError::NotUnicode(_)) => {
    // Err(format!(
    // "Invalid unicode in environment variable {}",
    // component_pidfile_env_name
    // ))
    // }
    // }?;
    // let pidfile = args.remove_one("pidfile")
    // .or(component_pidfile_env)
    // .map(PathBuf::from);
    let verbose = args.get_count("verbosity");
    let verbose_lvl: LevelFilter = match verbose {
        0 => Ok(LevelFilter::Warn),
        1 => Ok(LevelFilter::Info),
        2 => Ok(LevelFilter::Debug),
        3 => Ok(LevelFilter::Trace),
        _ => Err("More than three verbose flags is redundant")
    }?;
    let component_loglvl_env_name =
        format!("CONSTELLATION_{}_LOGLVL", S::COMPONENT_NAME.to_uppercase());
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
    let loglvl_env = match std::env::var(LOGLVL_ENV_NAME) {
        Ok(val) => {
            if verbose == 0 {
                Ok(Some(val))
            } else {
                Err(format!(
                    concat!(
                        "Cannot use verbose flag when setting ",
                        "log level through environment variable {}"
                    ),
                    LOGLVL_ENV_NAME
                ))
            }
        }
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "Invalid unicode in environment variable {}",
            LOGLVL_ENV_NAME
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

/// Set up command-line argument parser.
fn cmdargs_setup<S: Standalone>() -> Command {
    let mut cmd = command!()
        .version(S::VERSION)
        .arg(
            Arg::new("confdir")
                .short('c')
                .long("confdir")
                .help("Location of configuration files")
        )
        .arg(
            Arg::new("daemon")
                .short('d')
                .long("daemon")
                .help("Run as a daemon")
        )
        .arg(
            Arg::new("loglvl")
                .short('l')
                .long("loglvl")
                .value_parser(["error", "warn", "info", "debug", "trace"])
                .help("Set logging level")
        )
        .arg(
            Arg::new("pidfile")
                .long("pidfile")
                .help("Location of PID file")
        )
        .arg(
            Arg::new("verbosity")
                .short('v')
                .long("verbose")
                .help("Increase logging verbosity")
                .action(ArgAction::Count)
        );

    S::cmdargs(&mut cmd);

    cmd
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
