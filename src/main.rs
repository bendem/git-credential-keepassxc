mod cli;
mod config;
mod git;
mod keepassxc;
mod utils;

use anyhow::{anyhow, Result};
use clap::{App, ArgMatches};
use cli::UnlockOptions;
use config::{Caller, Config, Database};
use crypto_box::{PublicKey, SecretKey};
use git::GitCredentialMessage;
use keepassxc::{errors::*, messages::*, Group};
use once_cell::sync::OnceCell;
use slog::{Drain, Level, Logger};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::Duration;
use sysinfo::{get_current_pid, ProcessExt, System, SystemExt};
use utils::*;

static LOGGER: OnceCell<Logger> = OnceCell::new();

fn exchange_keys<T: AsRef<str>>(client_id: T, session_pubkey: &PublicKey) -> Result<PublicKey> {
    // exchange public keys
    let cpr_req = ChangePublicKeysRequest::new(client_id.as_ref(), session_pubkey);
    let cpr_resp = cpr_req.send()?;
    Ok(cpr_resp
        .get_public_key()
        .ok_or_else(|| anyhow!("Failed to retrieve host public key"))?)
}

fn start_session() -> Result<(String, SecretKey, PublicKey)> {
    // generate keys for encrypting current session
    let session_seckey = generate_secret_key();
    let session_pubkey = session_seckey.public_key();

    // temporary client id
    let (_, client_id) = nacl_nonce();

    // exchange public keys
    let host_pubkey = exchange_keys(&client_id, &session_pubkey)?;

    // initialise crypto_box
    let _ = get_client_box(Some(&host_pubkey), Some(&session_seckey));

    Ok((client_id, session_seckey, host_pubkey))
}

fn read_git_request() -> Result<(GitCredentialMessage, String)> {
    // read credential request
    let git_req = {
        let mut git_req_string = String::with_capacity(256);
        io::stdin().read_to_string(&mut git_req_string)?;
        GitCredentialMessage::from_str(&git_req_string)?
    };
    debug!("Git credential request: {:?}", git_req);
    let url = {
        if let Some(ref url_string) = git_req.url {
            url_string.clone()
        } else {
            if git_req.protocol.is_none() || git_req.host.is_none() {
                return Err(anyhow!(
                    "Protocol and host are both required when URL is not provided"
                ));
            }
            format!(
                "{}://{}/{}",
                git_req.protocol.clone().unwrap(),
                git_req.host.clone().unwrap(),
                git_req.path.clone().unwrap_or_else(|| "".to_owned())
            )
        }
    };
    Ok((git_req, url))
}

fn associated_databases<T: AsRef<str>>(
    config: &Config,
    client_id: T,
    unlock_options: &Option<UnlockOptions>,
) -> Result<Vec<Database>> {
    let databases: Vec<_> = config
        .get_databases()?
        .iter()
        .filter(|ref db| {
            let mut remain_retries = unlock_options.as_ref().map_or_else(|| 0, |v| v.max_retries);
            let mut success = false;
            loop {
                let taso_req = TestAssociateRequest::new(db.id.as_str(), db.pkey.as_str());
                // trigger unlock if command line argument is given
                let taso_resp = taso_req.send(client_id.as_ref(), unlock_options.is_some());
                let database_locked = match &taso_resp {
                    Ok(_) => false,
                    Err(e) => {
                        if let Some(keepass_error) = e.downcast_ref::<KeePassError>() {
                            keepass_error.is_database_locked()
                        } else {
                            false
                        }
                    }
                };
                if let Ok(ref taso_resp) = taso_resp {
                    success = taso_resp
                        .success
                        .clone()
                        .unwrap_or_else(|| KeePassBoolean(false))
                        .into();
                }
                if taso_resp.is_err() || !success {
                    warn!(
                        "Failed to authenticate against database {} using stored key",
                        db.id
                    );
                }
                if success || !database_locked || unlock_options.is_none() {
                    break;
                }
                // loop get-databasehash until unlocked
                while remain_retries > 0 || unlock_options.as_ref().unwrap().max_retries == 0 {
                    warn!(
                        "Database {} is locked, gonna retry in {}ms (Remaining: {})",
                        db.id,
                        unlock_options.as_ref().unwrap().interval,
                        remain_retries
                    );
                    thread::sleep(Duration::from_millis(
                        unlock_options.as_ref().unwrap().interval,
                    ));

                    let gh_req = GetDatabaseHashRequest::new();
                    if gh_req.send(client_id.as_ref(), false).is_ok() {
                        info!("Database {} is unlocked", db.id);
                        break;
                    }
                    if unlock_options.as_ref().unwrap().max_retries != 0 {
                        remain_retries -= 1;
                    }
                }
                // still not unlocked, break
                if remain_retries == 0 && unlock_options.as_ref().unwrap().max_retries != 0 {
                    break;
                }
            }
            success
        })
        .cloned()
        .collect();
    if databases.is_empty() {
        Err(anyhow!(
            "No valid database associations found in configuration file"
        ))
    } else {
        info!(
            "Successfully authenticated against {} database(s)",
            databases.len()
        );
        Ok(databases)
    }
}

fn handle_secondary_encryption(config_file: &mut Config) -> Result<()> {
    println!("There are existing encryption profile(s). If you'd like to reuse an existing encryption key, plug in the corresponding (hardware) token.");
    print!("Press Enter to continue... ");
    std::io::stdout().flush()?;
    std::io::stdin().read_line(&mut String::new())?;
    if config_file.get_encryption_key().is_err() {
        warn!("Failed to extract encryption key from existing profiles");
        println!("Failed to extract the encryption key! Continue to configure a new (hardware) token using a DIFFERENT encryption key.")
    }
    println!("Now make sure you've plugged in the (hardware) token you'd like to use.");
    print!("Press Enter to continue... ");
    std::io::stdout().flush()?;
    std::io::stdin().read_line(&mut String::new())?;
    Ok(())
}

fn configure<T: AsRef<Path>>(config_path: T, args: &ArgMatches) -> Result<()> {
    // start session
    let (client_id, session_seckey, _) = start_session()?;
    let session_pubkey = session_seckey.public_key();

    // generate permanent client key for future authentication
    let id_seckey = generate_secret_key();
    let id_pubkey = id_seckey.public_key();

    let aso_req = AssociateRequest::new(&session_pubkey, &id_pubkey);
    let aso_resp = aso_req.send(&client_id, false)?;
    let database_id = aso_resp.id.ok_or_else(|| anyhow!("Association failed"))?;

    // try to create a new group even if it already exists, KeePassXC will do the deduplication
    let group_name = args
        .subcommand_matches("configure")
        .and_then(|m| m.value_of("group"))
        .expect("Group name not specified (there's a default one though, bug?)");
    let cng_req = CreateNewGroupRequest::new(group_name);
    let cng_resp = cng_req.send(&client_id, false)?;
    let group = Group::new(cng_resp.name, cng_resp.uuid);

    // read existing or create new config
    let mut config_file = if let Ok(config_file) = Config::read_from(&config_path) {
        verify_caller(&config_file)?;
        config_file
    } else {
        Config::new()
    };

    let encryption = args
        .subcommand_matches("configure")
        .and_then(|m| m.value_of("encrypt"));
    if let Some(encryption) = encryption {
        if config_file.count_encryptions() > 0 && !encryption.is_empty() {
            handle_secondary_encryption(&mut config_file)?;
        }
        // this will error if an existing encryption profile has already been configured for the
        // underlying hardware/etc
        // in this case user should decrypt the configuration first
        config_file.add_encryption(encryption)?;
    }

    // save new config
    info!(
        "Saving configuration to {}",
        config_path.as_ref().to_string_lossy()
    );
    config_file.add_database(
        Database::new(database_id, id_seckey, group),
        encryption.is_some(),
    )?;
    config_file.write_to(&config_path)?;

    Ok(())
}

fn encrypt<T: AsRef<Path>>(config_path: T, args: &ArgMatches) -> Result<()> {
    let mut config_file = Config::read_from(&config_path)?;
    verify_caller(&config_file)?;

    let encryption = args
        .subcommand_matches("encrypt")
        .and_then(|m| m.value_of("ENCRYPTION_PROFILE"));

    let count_databases_to_encrypt =
        config_file.count_databases() - config_file.count_encrypted_databases();
    let count_callers_to_encrypt =
        config_file.count_callers() - config_file.count_encrypted_callers();
    if count_databases_to_encrypt == 0
        && count_callers_to_encrypt == 0
        && encryption.map(|m| m.is_empty()).unwrap_or_else(|| true)
    {
        warn!("Database and callers profiles have already been encrypted");
        return Ok(());
    }
    info!(
        "{} database profile(s) to encrypt",
        count_databases_to_encrypt
    );
    info!(
        "{} caller profile(s) to encrypt",
        count_databases_to_encrypt
    );

    if let Some(encryption) = encryption {
        if config_file.count_encryptions() > 0 && !encryption.is_empty() {
            handle_secondary_encryption(&mut config_file)?;
        }
        // this will error if an existing encryption profile has already been configured for the
        // underlying hardware/etc
        // in this case user should decrypt the configuration first
        config_file.add_encryption(encryption)?;
    }

    let count_databases_encrypted = config_file.encrypt_databases()?;
    let count_callers_encrypted = config_file.encrypt_callers()?;
    info!(
        "{} database profile(s) encrypted",
        count_databases_encrypted
    );
    info!("{} caller profile(s) encrypted", count_callers_encrypted);

    config_file.write_to(config_path)?;

    Ok(())
}

fn decrypt<T: AsRef<Path>>(config_path: T) -> Result<()> {
    let mut config_file = Config::read_from(&config_path)?;
    verify_caller(&config_file)?;

    let count_databases_to_decrypt = config_file.count_encrypted_databases();
    let count_callers_to_decrypt = config_file.count_encrypted_callers();
    if count_databases_to_decrypt == 0 && count_callers_to_decrypt == 0 {
        warn!("Database and callers profiles have already been decrypted");
        return Ok(());
    }
    info!(
        "{} database profile(s) to decrypt",
        count_databases_to_decrypt
    );
    info!("{} caller profile(s) to decrypt", count_callers_to_decrypt);

    config_file.decrypt_databases()?;
    config_file.decrypt_callers()?;
    if config_file.count_encrypted_databases() == 0 && config_file.count_encrypted_callers() == 0 {
        config_file.clear_encryptions();
    }

    config_file.write_to(config_path)?;

    Ok(())
}

fn caller<T: AsRef<Path>>(config_path: T, args: &ArgMatches) -> Result<()> {
    // read existing or create new config
    let mut config_file = if let Ok(config_file) = Config::read_from(&config_path) {
        verify_caller(&config_file)?;
        config_file
    } else {
        Config::new()
    };

    let subcommand = args.subcommand_matches("caller").unwrap();
    match subcommand.subcommand() {
        ("add", Some(add_args)) => {
            let path = add_args
                .value_of("PATH")
                .ok_or_else(|| anyhow!("Must specify path"))?;
            let caller = Caller {
                path: path.to_owned(),
                uid: if let Some(id) = add_args.value_of("uid") {
                    Some(u32::from_str(id).map_err(|_| anyhow!("Invalid UID"))?)
                } else {
                    None
                },
                gid: if let Some(id) = add_args.value_of("gid") {
                    Some(u32::from_str(id).map_err(|_| anyhow!("Invalid GID"))?)
                } else {
                    None
                },
            };
            let encryption = subcommand
                .subcommand_matches("add")
                .and_then(|m| m.value_of("encrypt"));
            if let Some(encryption) = encryption {
                // this will error if an existing encryption profile has already been configured for the
                // underlying hardware/etc
                // in this case user should decrypt the configuration first
                config_file.add_encryption(encryption)?;
            }
            config_file.add_caller(caller, encryption.is_some())?;
            config_file.write_to(config_path)
        }
        ("clear", _) => {
            config_file.clear_callers();
            config_file.write_to(config_path)
        }
        _ => Err(anyhow!("No subcommand selected")),
    }
}

fn verify_caller(config: &Config) -> Result<Option<(usize, PathBuf)>> {
    if config.count_callers() == 0
        && (cfg!(not(feature = "strict-caller")) || config.count_databases() == 0)
    {
        info!(
            "Caller verification skipped as no caller profiles defined and strict-caller disabled"
        );
        return Ok(None);
    }
    let pid = get_current_pid().map_err(|s| anyhow!("Failed to retrieve current PID: {}", s))?;
    info!("PID: {}", pid);
    let system = System::new_all();
    let proc = system
        .get_process(pid)
        .ok_or_else(|| anyhow!("Failed to retrieve information of current process"))?;
    let ppid = proc
        .parent()
        .ok_or_else(|| anyhow!("Failed to retrieve parent PID"))?;
    info!("PPID: {}", ppid);
    let pproc = system
        .get_process(ppid)
        .ok_or_else(|| anyhow!("Failed to retrieve parent process information"))?;
    let ppath = pproc.exe().to_string_lossy();
    info!("Parent process path: {}", ppath);
    let callers = config.get_callers()?;
    #[cfg(unix)]
    let matching_callers: Vec<_> = callers
        .iter()
        .filter(|caller| {
            caller.path == ppath
                && caller.uid.map(|id| id == proc.uid).unwrap_or(true)
                && caller.gid.map(|id| id == proc.gid).unwrap_or(true)
        })
        .collect();
    #[cfg(windows)]
    let matching_callers: Vec<_> = callers
        .iter()
        .filter(|caller| caller.path == ppath)
        .collect();
    if matching_callers.is_empty() {
        Err(anyhow!("You are not allowed to use this program"))
    } else {
        Ok(Some((ppid as usize, pproc.exe().to_owned())))
    }
}

/// Returns all entries from KeePassXC except for expired ones (which are not returned by KeePassXC
/// actually, but better to be safe than sorry)
fn get_logins_for<T: AsRef<str>>(
    config: &Config,
    client_id: T,
    url: T,
    unlock_options: &Option<UnlockOptions>,
) -> Result<Vec<LoginEntry>> {
    let databases = associated_databases(config, client_id.as_ref(), unlock_options)?;
    let id_key_pairs: Vec<_> = databases
        .iter()
        .map(|d| (d.id.as_str(), d.pkey.as_str()))
        .collect();

    // ask KeePassXC for logins
    let gl_req = GetLoginsRequest::new(url.as_ref(), None, None, &id_key_pairs[..]);
    let gl_resp = gl_req.send(client_id.as_ref(), false)?;

    let login_entries: Vec<_> = gl_resp
        .entries
        .into_iter()
        .filter(|e| e.expired.is_none() || !e.expired.as_ref().unwrap().0)
        .collect();
    Ok(login_entries)
}

fn filter_kph_logins(login_entries: &[LoginEntry]) -> (u32, Vec<&LoginEntry>) {
    let mut kph_false = 0u32;
    let login_entries: Vec<&LoginEntry> = login_entries
        .iter()
        .filter(|entry| {
            if let Some(ref string_fields) = entry.string_fields {
                let kph_false_fields = string_fields.iter().find(|m| {
                    if let Some(v) = m.get("KPH: git") {
                        v == "false"
                    } else {
                        false
                    }
                });
                if kph_false_fields.is_some() {
                    kph_false += 1;
                }
                kph_false_fields.is_none()
            } else {
                true
            }
        })
        .collect();
    (kph_false, login_entries)
}

fn get_logins<T: AsRef<Path>>(
    config_path: T,
    unlock_options: &Option<UnlockOptions>,
) -> Result<()> {
    let config = Config::read_from(config_path.as_ref())?;
    let _verify_caller = verify_caller(&config)?;
    // read credential request
    let (git_req, url) = read_git_request()?;

    #[cfg(feature = "notification")]
    {
        if let Some((ppid, ppath)) = _verify_caller {
            use notify_rust::{Notification, Timeout};
            let notification = Notification::new()
                .summary("Credential request")
                .body(&format!(
                    "{} ({}) has requested credential for {}",
                    ppath.file_name().unwrap_or_default().to_string_lossy(),
                    ppid,
                    url
                ))
                .timeout(Timeout::Milliseconds(6000))
                .show();
            if let Err(e) = notification {
                warn!("Failed to show notification for credential request, {}", e);
            }
        }
    }

    // start session
    let (client_id, _, _) = start_session()?;

    let login_entries = get_logins_for(&config, &client_id, &url, unlock_options)?;
    info!("KeePassXC return {} login(s)", login_entries.len());
    let (kph_false, mut login_entries) = filter_kph_logins(&login_entries);
    if kph_false > 0 {
        info!("{} login(s) were labeled as KPH: git == false", kph_false);
    }
    if login_entries.is_empty() {
        return Err(anyhow!("No matching logins found"));
    }
    if login_entries.len() > 1 && git_req.username.is_some() {
        let username = git_req.username.as_ref().unwrap();
        let login_entries_name_matches: Vec<_> = login_entries
            .iter()
            .filter(|entry| entry.login == *username)
            .cloned()
            .collect();
        if !login_entries_name_matches.is_empty() {
            info!(
                "{} login(s) left after filtering by username",
                login_entries_name_matches.len()
            );
            login_entries = login_entries_name_matches;
        }
    }
    if login_entries.len() > 1 {
        warn!("More than 1 matching logins found, only the first one will be returned");
    }

    let login = login_entries.first().unwrap();
    let mut git_resp = git_req;
    git_resp.username = Some(login.login.clone());
    git_resp.password = Some(login.password.clone());

    io::stdout().write_all(git_resp.to_string().as_bytes())?;

    Ok(())
}

fn store_login<T: AsRef<Path>>(
    config_path: T,
    unlock_options: &Option<UnlockOptions>,
) -> Result<()> {
    let config = Config::read_from(config_path.as_ref())?;
    verify_caller(&config)?;
    // read credential request
    let (git_req, url) = read_git_request()?;
    // start session
    let (client_id, _, _) = start_session()?;

    if git_req.username.is_none() {
        return Err(anyhow!("Username is missing"));
    }
    if git_req.password.is_none() {
        return Err(anyhow!("Password is missing"));
    }

    let login_entries =
        get_logins_for(&config, &client_id, &url, unlock_options).and_then(|entries| {
            let (kph_false, entries) = filter_kph_logins(&entries);
            if kph_false > 0 {
                info!("{} login(s) were labeled as KPH: git == false", kph_false);
            }
            let username = git_req.username.as_ref().unwrap();
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|entry| entry.login == *username)
                .cloned()
                .collect();
            info!(
                "{} login(s) left after filtering by username",
                entries.len()
            );
            if entries.is_empty() {
                // this Err is never used
                Err(anyhow!(
                    "No remaining logins after filtering out {} KPH: git == false one(s)",
                    kph_false
                ))
            } else {
                Ok(entries)
            }
        });

    let sl_req = if let Ok(login_entries) = login_entries {
        if login_entries.len() == 1 {
            warn!("Existing login found, gonna update the entry");
        } else {
            warn!("More than 1 existing logins found, gonna update the first entry");
        }
        let login_entry = login_entries.first().unwrap();

        if &login_entry.login == git_req.username.as_ref().unwrap()
            && &login_entry.password == git_req.password.as_ref().unwrap()
        {
            // KeePassXC treats this as error, and Git sometimes does this as the operation should
            // be idempotent
            return Ok(());
        }

        let databases = config.get_databases()?;
        if databases.len() > 1 {
            // how do I know which database it's from?
            error!(
                "Trying to update an existing login when multiple databases are configured, this is not implemented yet"
            );
            unimplemented!();
        }
        let database = databases.first().unwrap();
        SetLoginRequest::new(
            &url,
            &url,
            &database.id,
            &git_req.username.unwrap(),
            &git_req.password.unwrap(),
            Some(&database.group),
            Some(&database.group_uuid), // KeePassXC won't move the existing entry though
            Some(&login_entry.uuid),
        )
    } else {
        info!("No existing logins found, gonna create a new one");
        let databases = config.get_databases()?;
        if databases.len() > 1 {
            warn!(
                "More than 1 databases configured, gonna save the new login in the first database"
            );
        }
        let database = databases.first().unwrap();
        SetLoginRequest::new(
            &url,
            &url,
            &database.id,
            &git_req.username.unwrap(),
            &git_req.password.unwrap(),
            Some(&database.group),
            Some(&database.group_uuid),
            None,
        )
    };
    let sl_resp = sl_req.send(&client_id, false)?;
    if let Some(success) = sl_resp.success {
        // wtf?!?!
        if success.0
            && (sl_resp.error.is_none()
                || sl_resp.error.as_ref().unwrap().is_empty()
                || sl_resp.error.as_ref().unwrap() == "success")
        {
            Ok(())
        } else {
            error!(
                "Failed to store login. Error: {}, Error Code: {}",
                sl_resp.error.unwrap_or_else(|| "N/A".to_owned()),
                sl_resp.error_code.unwrap_or_else(|| "N/A".to_owned())
            );
            Err(anyhow!("Failed to store login"))
        }
    } else {
        error!("Set login request failed");
        Err(anyhow!("Set login request failed"))
    }
}

fn erase_login() -> Result<()> {
    // Don't treat this as error as when server rejects a login Git may try to erase it. This is
    // not desirable since sometimes it's merely a configuration issue, e.g. a lot of Git servers
    // reject logins over HTTP(S) when SSH keys have been uploaded
    error!("KeePassXC doesn't allow erasing logins via socket at the time of writing");
    let _ = read_git_request();
    Ok(())
}

fn real_main() -> Result<()> {
    #[cfg(all(target_os = "linux", not(debug_assertions)))]
    {
        prctl::set_dumpable(false)
            .or_else(|c| Err(anyhow!("Failed to disable dump, code: {}", c)))?;
    }

    let yaml = clap::load_yaml!("cli.yml");
    let args = App::from_yaml(yaml)
        .author(env!("CARGO_PKG_AUTHORS"))
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();

    let level = Level::from_usize(std::cmp::min(6, args.occurrences_of("verbose") + 2) as usize)
        .unwrap_or(Level::Error);
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator)
        .build()
        .filter_level(level)
        .fuse();
    let drain = std::sync::Mutex::new(drain).fuse();
    let logger = Logger::root(drain, slog::o!());
    LOGGER
        .set(logger)
        .map_err(|_| anyhow!("Failed to initialise logger"))?;

    #[cfg(all(target_os = "linux", not(debug_assertions)))]
    {
        if let Ok(dumpable) = prctl::get_dumpable() {
            if dumpable {
                error!("Failed to disable dump");
            } else {
                info!("Dump is disabled");
            }
        } else {
            error!("Failed to query dumpable status");
        }
    }

    let config_path = {
        if let Some(path) = args.value_of("config") {
            info!("Configuration file path is set to {} by user", path);
            PathBuf::from(path)
        } else {
            let base_dirs = directories_next::BaseDirs::new()
                .ok_or_else(|| anyhow!("Failed to initialise base_dirs"))?;
            base_dirs.config_dir().join(clap::crate_name!())
        }
    };
    if let Some(path) = args.value_of("socket") {
        info!("Socket path is set to {} by user", path);
        let path = PathBuf::from(path);
        utils::SOCKET_PATH.with(|s| {
            s.set(path).expect("Failed to set socket path, bug?");
        });
    };
    let unlock_options = {
        if let Some(unlock_options) = args.value_of("unlock") {
            info!("Database unlock option is given by user");
            Some(UnlockOptions::from_str(unlock_options)?)
        } else {
            None
        }
    };

    let subcommand = args
        .subcommand_name()
        .ok_or_else(|| anyhow!("No subcommand selected"))?;
    debug!("Subcommand: {}", subcommand);
    match subcommand {
        "configure" => configure(config_path, &args),
        "encrypt" => encrypt(config_path, &args),
        "decrypt" => decrypt(config_path),
        "caller" => caller(config_path, &args),
        "get" => get_logins(config_path, &unlock_options),
        "store" => store_login(config_path, &unlock_options),
        "erase" => erase_login(),
        _ => Err(anyhow!(anyhow!("Unrecognised subcommand"))),
    }
}

fn main() {
    if let Err(e) = real_main() {
        let source = e
            .source()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        error!("{}, Caused by: {}", e, source);
    }
}
