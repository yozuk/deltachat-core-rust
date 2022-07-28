//! # Import/export module.

use std::any::Any;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ::pgp::types::KeyTrait;
use anyhow::{bail, ensure, format_err, Context as _, Result};
use futures::{StreamExt, TryStreamExt};
use futures_lite::FutureExt;
use rand::{thread_rng, Rng};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio_tar::Archive;

use crate::blob::BlobObject;
use crate::chat::{self, delete_and_reset_all_device_msgs, ChatId};
use crate::config::Config;
use crate::contact::ContactId;
use crate::context::Context;
use crate::e2ee;
use crate::events::EventType;
use crate::key::{self, DcKey, DcSecretKey, SignedPublicKey, SignedSecretKey};
use crate::log::LogExt;
use crate::message::{Message, MsgId, Viewtype};
use crate::mimeparser::SystemMessage;
use crate::param::Param;
use crate::pgp;
use crate::sql;
use crate::stock_str;
use crate::tools::{
    create_folder, delete_file, get_filesuffix_lc, open_file_std, read_file, time, write_file,
    EmailAddress,
};

// Name of the database file in the backup.
const DBFILE_BACKUP_NAME: &str = "dc_database_backup.sqlite";
const BLOBS_BACKUP_NAME: &str = "blobs_backup";

#[derive(Debug, Display, Copy, Clone, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum ImexMode {
    /// Export all private keys and all public keys of the user to the
    /// directory given as `path`.  The default key is written to the files `public-key-default.asc`
    /// and `private-key-default.asc`, if there are more keys, they are written to files as
    /// `public-key-<id>.asc` and `private-key-<id>.asc`
    ExportSelfKeys = 1,

    /// Import private keys found in the directory given as `path`.
    /// The last imported key is made the default keys unless its name contains the string `legacy`.
    /// Public keys are not imported.
    ImportSelfKeys = 2,

    /// Export a backup to the directory given as `path` with the given `passphrase`.
    /// The backup contains all contacts, chats, images and other data and device independent settings.
    /// The backup does not contain device dependent settings as ringtones or LED notification settings.
    /// The name of the backup is typically `delta-chat-<day>.tar`, if more than one backup is create on a day,
    /// the format is `delta-chat-<day>-<number>.tar`
    ExportBackup = 11,

    /// `path` is the file (not: directory) to import. The file is normally
    /// created by DC_IMEX_EXPORT_BACKUP and detected by imex_has_backup(). Importing a backup
    /// is only possible as long as the context is not configured or used in another way.
    ImportBackup = 12,
}

/// Import/export things.
///
/// What to do is defined by the `what` parameter.
///
/// During execution of the job,
/// some events are sent out:
///
/// - A number of `DC_EVENT_IMEX_PROGRESS` events are sent and may be used to create
///   a progress bar or stuff like that. Moreover, you'll be informed when the imex-job is done.
///
/// - For each file written on export, the function sends `DC_EVENT_IMEX_FILE_WRITTEN`
///
/// Only one import-/export-progress can run at the same time.
/// To cancel an import-/export-progress, drop the future returned by this function.
pub async fn imex(
    context: &Context,
    what: ImexMode,
    path: &Path,
    passphrase: Option<String>,
) -> Result<()> {
    let cancel = context.alloc_ongoing().await?;

    let res = imex_inner(context, what, path, passphrase)
        .race(async {
            cancel.recv().await.ok();
            Err(format_err!("canceled"))
        })
        .await;

    context.free_ongoing().await;

    if let Err(err) = res.as_ref() {
        // We are using Anyhow's .context() and to show the inner error, too, we need the {:#}:
        error!(context, "IMEX failed to complete: {:#}", err);
        context.emit_event(EventType::ImexProgress(0));
    } else {
        info!(context, "IMEX successfully completed");
        context.emit_event(EventType::ImexProgress(1000));
    }

    res
}

pub async fn receive_backup(
    context: &Context,
    ticket_bytes: Vec<u8>,
    passphrase: Option<String>,
) -> Result<()> {
    let cancel = context.alloc_ongoing().await?;

    let res = receive_backup_inner(context, ticket_bytes, passphrase.unwrap_or_default())
        .race(async {
            cancel.recv().await.ok();
            Err(format_err!("canceled"))
        })
        .await;

    context.free_ongoing().await;

    if let Err(err) = res.as_ref() {
        // We are using Anyhow's .context() and to show the inner error, too, we need the {:#}:
        error!(context, "IMEX failed to complete: {:#}", err);
        context.emit_event(EventType::ImexProgress(0));
    } else {
        info!(context, "IMEX successfully completed");
        context.emit_event(EventType::ImexProgress(1000));
    }

    res
}

pub async fn receive_backup_inner(
    context: &Context,
    ticket_bytes: Vec<u8>,
    passphrase: String,
) -> Result<()> {
    use iroh_share::{Receiver, Ticket};
    ensure!(
        !context.is_configured().await?,
        "Cannot import backups to accounts in use."
    );
    ensure!(
        context.scheduler.read().await.is_none(),
        "cannot import backup, IO is running"
    );
    let ticket = Ticket::from_bytes(&ticket_bytes)?;

    let sender_dir = tempfile::tempdir().unwrap();
    let sender_db = sender_dir.path().join("db");

    let port = 9991;
    let rpc_p2p_port = 5551;
    let rpc_store_port = 5561;
    let receiver = Receiver::new(port, rpc_p2p_port, rpc_store_port, &sender_db)
        .await
        .context("failed to create sender")?;
    let mut receiver_transfer = receiver
        .transfer_from_ticket(&ticket)
        .await
        .context("failed to read transfer")?;
    let data = receiver_transfer.recv().await?;
    let progress = receiver_transfer.progress()?;

    // progress report
    let ctx = context.clone();
    let progress_task = tokio::spawn(async move {
        let mut last_progress = 0;
        while let Ok(ev) = progress.recv().await {
            match ev {
                Ok(iroh_share::ProgressEvent::Piece { index, total }) => {
                    let progress = 1000 * index / total;
                    if progress != last_progress && progress < 1000 {
                        ctx.emit_event(EventType::ImexProgress(progress));
                        last_progress = progress;
                    }
                }
                Err(err) => {
                    error!(ctx, "IMEX receive backup failed to complete: {}", err);
                    ctx.emit_event(EventType::ImexProgress(0));
                }
            }
        }
    });

    let out = context.get_blobdir();

    for link in data.read_dir().unwrap() {
        let link = link?;
        let file_content = data.read_file(&link).await?;
        let name = link.name.unwrap_or_default();
        let path = out.join(&name);
        println!("Writing {}", path.display());
        let mut file = tokio::fs::File::create(&path)
            .await
            .with_context(|| format!("create file: {}", path.display()))?;
        let mut content = file_content.pretty();
        tokio::io::copy(&mut content, &mut file)
            .await
            .context("copy")?;
        file.flush().await?;

        if name == DBFILE_BACKUP_NAME {
            let unpacked_database = context.get_blobdir().join(DBFILE_BACKUP_NAME);
            context
                .sql
                .import(&unpacked_database, passphrase.clone())
                .await
                .context("cannot import unpacked database")?;
            fs::remove_file(unpacked_database)
                .await
                .context("cannot remove unpacked database")?;
        } else {
            // nothing to do, unpacked directly into the blobs dir
        }
    }

    println!("Received all data, written to: {}", out.display());
    receiver.close().await?;
    progress_task.await?;

    Ok(())
}

pub async fn send_backup(
    context: &Context,
    path: &Path,
    passphrase: Option<String>,
) -> Result<(iroh_share::Sender, iroh_share::SenderTransfer)> {
    let cancel = context.alloc_ongoing().await?;

    let res = send_backup_inner(context, path, passphrase)
        .race(async {
            cancel.recv().await.ok();
            Err(format_err!("canceled"))
        })
        .await;

    context.free_ongoing().await;

    if let Err(err) = res.as_ref() {
        // We are using Anyhow's .context() and to show the inner error, too, we need the {:#}:
        error!(context, "IMEX failed to complete: {:#}", err);
        context.emit_event(EventType::ImexProgress(0));
    } else {
        info!(context, "IMEX successfully completed");
        context.emit_event(EventType::ImexProgress(1000));
    }

    res
}

async fn send_backup_inner(
    context: &Context,
    path: &Path,
    passphrase: Option<String>,
) -> Result<(iroh_share::Sender, iroh_share::SenderTransfer)> {
    info!(context, "Import/export dir: {}", path.display());
    ensure!(context.sql.is_open().await, "Database not opened.");
    context.emit_event(EventType::ImexProgress(10));

    // before we export anything, make sure the private key exists
    if e2ee::ensure_secret_key_exists(context).await.is_err() {
        bail!("Cannot create private key or private key not available.");
    } else {
        create_folder(context, &path).await?;
    }

    export_backup_iroh(context, path, passphrase.unwrap_or_default()).await
}

/// Returns the filename of the backup found (otherwise an error)
pub async fn has_backup(_context: &Context, dir_name: &Path) -> Result<String> {
    let mut dir_iter = tokio::fs::read_dir(dir_name).await?;
    let mut newest_backup_name = "".to_string();
    let mut newest_backup_path: Option<PathBuf> = None;

    while let Ok(Some(dirent)) = dir_iter.next_entry().await {
        let path = dirent.path();
        let name = dirent.file_name();
        let name: String = name.to_string_lossy().into();
        if name.starts_with("delta-chat")
            && name.ends_with(".tar")
            && (newest_backup_name.is_empty() || name > newest_backup_name)
        {
            // We just use string comparison to determine which backup is newer.
            // This works fine because the filenames have the form ...delta-chat-backup-2020-07-24-00.tar
            newest_backup_path = Some(path);
            newest_backup_name = name;
        }
    }

    match newest_backup_path {
        Some(path) => Ok(path.to_string_lossy().into_owned()),
        None => bail!("no backup found in {}", dir_name.display()),
    }
}

/// Initiates key transfer via Autocrypt Setup Message.
pub async fn initiate_key_transfer(context: &Context) -> Result<String> {
    use futures::future::FutureExt;

    let cancel = context.alloc_ongoing().await?;
    let res = do_initiate_key_transfer(context)
        .race(cancel.recv().map(|_| Err(format_err!("canceled"))))
        .await;

    context.free_ongoing().await;
    res
}

async fn do_initiate_key_transfer(context: &Context) -> Result<String> {
    let setup_code = create_setup_code(context);
    /* this may require a keypair to be created. this may take a second ... */
    let setup_file_content = render_setup_file(context, &setup_code).await?;
    /* encrypting may also take a while ... */
    let setup_file_blob = BlobObject::create(
        context,
        "autocrypt-setup-message.html",
        setup_file_content.as_bytes(),
    )
    .await?;

    let chat_id = ChatId::create_for_contact(context, ContactId::SELF).await?;
    let mut msg = Message {
        viewtype: Viewtype::File,
        ..Default::default()
    };
    msg.param.set(Param::File, setup_file_blob.as_name());
    msg.subject = stock_str::ac_setup_msg_subject(context).await;
    msg.param
        .set(Param::MimeType, "application/autocrypt-setup");
    msg.param.set_cmd(SystemMessage::AutocryptSetupMessage);
    msg.force_plaintext();
    msg.param.set_int(Param::SkipAutocrypt, 1);

    let msg_id = chat::send_msg(context, chat_id, &mut msg).await?;
    info!(context, "Wait for setup message being sent ...",);
    while !context.shall_stop_ongoing().await {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Ok(msg) = Message::load_from_db(context, msg_id).await {
            if msg.is_sent() {
                info!(context, "... setup message sent.",);
                break;
            }
        }
    }
    // no maybe_add_bcc_self_device_msg() here.
    // the ui shows the dialog with the setup code on this device,
    // it would be too much noise to have two things popping up at the same time.
    // maybe_add_bcc_self_device_msg() is called on the other device
    // once the transfer is completed.
    Ok(setup_code)
}

/// Renders HTML body of a setup file message.
///
/// The `passphrase` must be at least 2 characters long.
pub async fn render_setup_file(context: &Context, passphrase: &str) -> Result<String> {
    let passphrase_begin = if let Some(passphrase_begin) = passphrase.get(..2) {
        passphrase_begin
    } else {
        bail!("Passphrase must be at least 2 chars long.");
    };
    let private_key = SignedSecretKey::load_self(context).await?;
    let ac_headers = match context.get_config_bool(Config::E2eeEnabled).await? {
        false => None,
        true => Some(("Autocrypt-Prefer-Encrypt", "mutual")),
    };
    let private_key_asc = private_key.to_asc(ac_headers);
    let encr = pgp::symm_encrypt(passphrase, private_key_asc.as_bytes()).await?;

    let replacement = format!(
        concat!(
            "-----BEGIN PGP MESSAGE-----\r\n",
            "Passphrase-Format: numeric9x4\r\n",
            "Passphrase-Begin: {}"
        ),
        passphrase_begin
    );
    let pgp_msg = encr.replace("-----BEGIN PGP MESSAGE-----", &replacement);

    let msg_subj = stock_str::ac_setup_msg_subject(context).await;
    let msg_body = stock_str::ac_setup_msg_body(context).await;
    let msg_body_html = msg_body.replace('\r', "").replace('\n', "<br>");
    Ok(format!(
        concat!(
            "<!DOCTYPE html>\r\n",
            "<html>\r\n",
            "  <head>\r\n",
            "    <title>{}</title>\r\n",
            "  </head>\r\n",
            "  <body>\r\n",
            "    <h1>{}</h1>\r\n",
            "    <p>{}</p>\r\n",
            "    <pre>\r\n{}\r\n</pre>\r\n",
            "  </body>\r\n",
            "</html>\r\n"
        ),
        msg_subj, msg_subj, msg_body_html, pgp_msg
    ))
}

pub fn create_setup_code(_context: &Context) -> String {
    let mut random_val: u16;
    let mut rng = thread_rng();
    let mut ret = String::new();

    for i in 0..9 {
        loop {
            random_val = rng.gen();
            if random_val as usize <= 60000 {
                break;
            }
        }
        random_val = (random_val as usize % 10000) as u16;
        ret += &format!(
            "{}{:04}",
            if 0 != i { "-" } else { "" },
            random_val as usize
        );
    }

    ret
}

async fn maybe_add_bcc_self_device_msg(context: &Context) -> Result<()> {
    if !context.sql.get_raw_config_bool("bcc_self").await? {
        let mut msg = Message::new(Viewtype::Text);
        // TODO: define this as a stockstring once the wording is settled.
        msg.text = Some(
            "It seems you are using multiple devices with Delta Chat. Great!\n\n\
             If you also want to synchronize outgoing messages accross all devices, \
             go to the settings and enable \"Send copy to self\"."
                .to_string(),
        );
        chat::add_device_msg(context, Some("bcc-self-hint"), Some(&mut msg)).await?;
    }
    Ok(())
}

pub async fn continue_key_transfer(
    context: &Context,
    msg_id: MsgId,
    setup_code: &str,
) -> Result<()> {
    ensure!(!msg_id.is_special(), "wrong id");

    let msg = Message::load_from_db(context, msg_id).await?;
    ensure!(
        msg.is_setupmessage(),
        "Message is no Autocrypt Setup Message."
    );

    if let Some(filename) = msg.get_file(context) {
        let file = open_file_std(context, filename)?;
        let sc = normalize_setup_code(setup_code);
        let armored_key = decrypt_setup_file(&sc, file).await?;
        set_self_key(context, &armored_key, true, true).await?;
        maybe_add_bcc_self_device_msg(context).await?;

        Ok(())
    } else {
        bail!("Message is no Autocrypt Setup Message.");
    }
}

async fn set_self_key(
    context: &Context,
    armored: &str,
    set_default: bool,
    prefer_encrypt_required: bool,
) -> Result<()> {
    // try hard to only modify key-state
    let (private_key, header) = SignedSecretKey::from_asc(armored)?;
    let public_key = private_key.split_public_key()?;
    let preferencrypt = header.get("Autocrypt-Prefer-Encrypt");
    match preferencrypt.map(|s| s.as_str()) {
        Some(headerval) => {
            let e2ee_enabled = match headerval {
                "nopreference" => 0,
                "mutual" => 1,
                _ => {
                    bail!("invalid Autocrypt-Prefer-Encrypt header: {:?}", header);
                }
            };
            context
                .sql
                .set_raw_config_int("e2ee_enabled", e2ee_enabled)
                .await?;
        }
        None => {
            if prefer_encrypt_required {
                bail!("missing Autocrypt-Prefer-Encrypt header");
            }
        }
    };

    let self_addr = context.get_primary_self_addr().await?;
    let addr = EmailAddress::new(&self_addr)?;
    let keypair = pgp::KeyPair {
        addr,
        public: public_key,
        secret: private_key,
    };
    key::store_self_keypair(
        context,
        &keypair,
        if set_default {
            key::KeyPairUse::Default
        } else {
            key::KeyPairUse::ReadOnly
        },
    )
    .await?;

    info!(context, "stored self key: {:?}", keypair.secret.key_id());
    Ok(())
}

async fn decrypt_setup_file<T: std::io::Read + std::io::Seek>(
    passphrase: &str,
    file: T,
) -> Result<String> {
    let plain_bytes = pgp::symm_decrypt(passphrase, file).await?;
    let plain_text = std::string::String::from_utf8(plain_bytes)?;

    Ok(plain_text)
}

fn normalize_setup_code(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if ('0'..='9').contains(&c) {
            out.push(c);
            if let 4 | 9 | 14 | 19 | 24 | 29 | 34 | 39 = out.len() {
                out += "-"
            }
        }
    }
    out
}

async fn imex_inner(
    context: &Context,
    what: ImexMode,
    path: &Path,
    passphrase: Option<String>,
) -> Result<()> {
    info!(context, "Import/export dir: {}", path.display());
    ensure!(context.sql.is_open().await, "Database not opened.");
    context.emit_event(EventType::ImexProgress(10));

    if what == ImexMode::ExportBackup || what == ImexMode::ExportSelfKeys {
        // before we export anything, make sure the private key exists
        if e2ee::ensure_secret_key_exists(context).await.is_err() {
            bail!("Cannot create private key or private key not available.");
        } else {
            create_folder(context, &path).await?;
        }
    }

    match what {
        ImexMode::ExportSelfKeys => export_self_keys(context, path).await,
        ImexMode::ImportSelfKeys => import_self_keys(context, path).await,

        ImexMode::ExportBackup => {
            export_backup(context, path, passphrase.unwrap_or_default()).await
        }
        ImexMode::ImportBackup => {
            import_backup(context, path, passphrase.unwrap_or_default()).await?;
            context.sql.run_migrations(context).await
        }
    }
}

/// Imports backup into the currently open database.
///
/// The contents of the currently open database will be lost.
///
/// `passphrase` is the passphrase used to open backup database. If backup is unencrypted, pass
/// empty string here.
async fn import_backup(
    context: &Context,
    backup_to_import: &Path,
    passphrase: String,
) -> Result<()> {
    ensure!(
        !context.is_configured().await?,
        "Cannot import backups to accounts in use."
    );
    ensure!(
        context.scheduler.read().await.is_none(),
        "cannot import backup, IO is running"
    );

    let backup_file = File::open(backup_to_import).await?;
    let file_size = backup_file.metadata().await?.len();
    info!(
        context,
        "Import \"{}\" ({} bytes) to \"{}\".",
        backup_to_import.display(),
        file_size,
        context.get_dbfile().display()
    );

    context.sql.config_cache.write().await.clear();

    let mut archive = Archive::new(backup_file);

    let mut entries = archive.entries()?;
    let mut last_progress = 0;
    while let Some(file) = entries.next().await {
        let f = &mut file?;

        let current_pos = f.raw_file_position();
        let progress = 1000 * current_pos / file_size;
        if progress != last_progress && progress > 10 && progress < 1000 {
            // We already emitted ImexProgress(10) above
            context.emit_event(EventType::ImexProgress(progress as usize));
            last_progress = progress;
        }

        if f.path()?.file_name() == Some(OsStr::new(DBFILE_BACKUP_NAME)) {
            // async_tar can't unpack to a specified file name, so we just unpack to the blobdir and then move the unpacked file.
            f.unpack_in(context.get_blobdir()).await?;
            let unpacked_database = context.get_blobdir().join(DBFILE_BACKUP_NAME);
            context
                .sql
                .import(&unpacked_database, passphrase.clone())
                .await
                .context("cannot import unpacked database")?;
            fs::remove_file(unpacked_database)
                .await
                .context("cannot remove unpacked database")?;
        } else {
            // async_tar will unpack to blobdir/BLOBS_BACKUP_NAME, so we move the file afterwards.
            f.unpack_in(context.get_blobdir()).await?;
            let from_path = context.get_blobdir().join(f.path()?);
            if from_path.is_file() {
                if let Some(name) = from_path.file_name() {
                    fs::rename(&from_path, context.get_blobdir().join(name)).await?;
                } else {
                    warn!(context, "No file name");
                }
            }
        }
    }

    delete_and_reset_all_device_msgs(context).await?;

    Ok(())
}

/*******************************************************************************
 * Export backup
 ******************************************************************************/

/// Returns Ok((temp_db_path, temp_path, dest_path)) on success. Unencrypted database can be
/// written to temp_db_path. The backup can then be written to temp_path. If the backup succeeded,
/// it can be renamed to dest_path. This guarantees that the backup is complete.
fn get_next_backup_path(folder: &Path, backup_time: i64) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let folder = PathBuf::from(folder);
    let stem = chrono::NaiveDateTime::from_timestamp(backup_time, 0)
        // Don't change this file name format, in `dc_imex_has_backup` we use string comparison to determine which backup is newer:
        .format("delta-chat-backup-%Y-%m-%d")
        .to_string();

    // 64 backup files per day should be enough for everyone
    for i in 0..64 {
        let mut tempdbfile = folder.clone();
        tempdbfile.push(format!("{}-{:02}.db", stem, i));

        let mut tempfile = folder.clone();
        tempfile.push(format!("{}-{:02}.tar.part", stem, i));

        let mut destfile = folder.clone();
        destfile.push(format!("{}-{:02}.tar", stem, i));

        if !tempdbfile.exists() && !tempfile.exists() && !destfile.exists() {
            return Ok((tempdbfile, tempfile, destfile));
        }
    }
    bail!("could not create backup file, disk full?");
}

async fn export_backup(context: &Context, dir: &Path, passphrase: String) -> Result<()> {
    // get a fine backup file name (the name includes the date so that multiple backup instances are possible)
    let now = time();
    let (temp_db_path, temp_path, dest_path) = get_next_backup_path(dir, now)?;
    let _d1 = DeleteOnDrop(temp_db_path.clone());
    let _d2 = DeleteOnDrop(temp_path.clone());

    context
        .sql
        .set_raw_config_int("backup_time", now as i32)
        .await?;
    sql::housekeeping(context).await.ok_or_log(context);

    context
        .sql
        .execute("VACUUM;", paramsv![])
        .await
        .map_err(|e| warn!(context, "Vacuum failed, exporting anyway {}", e))
        .ok();

    ensure!(
        context.scheduler.read().await.is_none(),
        "cannot export backup, IO is running"
    );

    info!(
        context,
        "Backup '{}' to '{}'.",
        context.get_dbfile().display(),
        dest_path.display(),
    );

    context
        .sql
        .export(&temp_db_path, passphrase)
        .await
        .with_context(|| format!("failed to backup plaintext database to {:?}", temp_db_path))?;

    let res = export_backup_inner(context, &temp_db_path, &temp_path).await;

    match &res {
        Ok(_) => {
            fs::rename(temp_path, &dest_path).await?;
            context.emit_event(EventType::ImexFileWritten(dest_path));
        }
        Err(e) => {
            error!(context, "backup failed: {}", e);
        }
    }

    res
}
struct DeleteOnDrop(PathBuf);
impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        let file = self.0.clone();
        // Not using `tools::delete_file` here because it would send a DeletedBlobFile event
        // Hack to avoid panic in nested runtime calls of tokio
        std::fs::remove_file(file).ok();
    }
}

async fn export_backup_inner(
    context: &Context,
    temp_db_path: &Path,
    temp_path: &Path,
) -> Result<()> {
    let file = File::create(temp_path).await?;

    let mut builder = tokio_tar::Builder::new(file);

    builder
        .append_path_with_name(temp_db_path, DBFILE_BACKUP_NAME)
        .await?;

    let read_dir: Vec<_> =
        tokio_stream::wrappers::ReadDirStream::new(fs::read_dir(context.get_blobdir()).await?)
            .try_collect()
            .await?;
    let count = read_dir.len();
    let mut written_files = 0;

    let mut last_progress = 0;
    for entry in read_dir.into_iter() {
        let name = entry.file_name();
        if !entry.file_type().await?.is_file() {
            warn!(
                context,
                "Export: Found dir entry {} that is not a file, ignoring",
                name.to_string_lossy()
            );
            continue;
        }
        let mut file = File::open(entry.path()).await?;
        let path_in_archive = PathBuf::from(BLOBS_BACKUP_NAME).join(name);
        builder.append_file(path_in_archive, &mut file).await?;

        written_files += 1;
        let progress = 1000 * written_files / count;
        if progress != last_progress && progress > 10 && progress < 1000 {
            // We already emitted ImexProgress(10) above
            context.emit_event(EventType::ImexProgress(progress));
            last_progress = progress;
        }
    }

    builder.finish().await?;
    Ok(())
}

async fn export_backup_iroh(
    context: &Context,
    dir: &Path,
    passphrase: String,
) -> Result<(iroh_share::Sender, iroh_share::SenderTransfer)> {
    // get a fine backup file name (the name includes the date so that multiple backup instances are possible)
    let now = time();
    let (temp_db_path, temp_path, dest_path) = get_next_backup_path(dir, now)?;
    let sender_db_path = dir.join("iroh_db");
    let _d1 = DeleteOnDrop(temp_db_path.clone());
    let _d2 = DeleteOnDrop(temp_path.clone());
    let _d3 = DeleteOnDrop(sender_db_path.clone());

    context
        .sql
        .set_raw_config_int("backup_time", now as i32)
        .await?;
    sql::housekeeping(context).await.ok_or_log(context);

    context
        .sql
        .execute("VACUUM;", paramsv![])
        .await
        .map_err(|e| warn!(context, "Vacuum failed, exporting anyway {}", e))
        .ok();

    ensure!(
        context.scheduler.read().await.is_none(),
        "cannot export backup, IO is running"
    );

    info!(
        context,
        "Backup '{}' to '{}'.",
        context.get_dbfile().display(),
        dest_path.display(),
    );

    context
        .sql
        .export(&temp_db_path, passphrase)
        .await
        .with_context(|| format!("failed to backup plaintext database to {:?}", temp_db_path))?;

    let res = export_backup_iroh_inner(context, &temp_db_path, &temp_path).await;

    match res {
        Ok(dir_builder) => {
            let port = 9990;
            let rpc_p2p_port = 5550;
            let rpc_store_port = 5560;

            let sender =
                iroh_share::Sender::new(port, rpc_p2p_port, rpc_store_port, &sender_db_path)
                    .await?;
            let transfer = sender.transfer_from_dir_builder(dir_builder).await?;

            Ok((sender, transfer))
        }
        Err(e) => {
            error!(context, "backup failed: {}", e);
            Err(e)
        }
    }
}

async fn export_backup_iroh_inner(
    context: &Context,
    temp_db_path: &Path,
    temp_path: &Path,
) -> Result<iroh_resolver::unixfs_builder::DirectoryBuilder> {
    use iroh_resolver::unixfs_builder::{DirectoryBuilder, FileBuilder};

    let mut dir_builder = DirectoryBuilder::new();
    dir_builder.name(
        temp_path
            .file_name()
            .map(|s| s.to_string_lossy().to_owned())
            .unwrap_or_default(),
    );

    {
        let mut file = FileBuilder::new();
        let db_content = tokio::fs::File::open(temp_db_path).await?;
        file.name(DBFILE_BACKUP_NAME).content_reader(db_content);

        dir_builder.add_file(file.build().await?);
    }

    let read_dir: Vec<_> =
        tokio_stream::wrappers::ReadDirStream::new(fs::read_dir(context.get_blobdir()).await?)
            .try_collect()
            .await?;
    let count = read_dir.len();
    let mut written_files = 0;

    let mut last_progress = 0;
    for entry in read_dir.into_iter() {
        let name = entry.file_name();
        if !entry.file_type().await?.is_file() {
            warn!(
                context,
                "Export: Found dir entry {} that is not a file, ignoring",
                name.to_string_lossy()
            );
            continue;
        }
        let file_content = File::open(entry.path()).await?;

        {
            let mut file = FileBuilder::new();
            file.name(name.to_string_lossy().to_owned());
            file.content_reader(file_content);
            dir_builder.add_file(file.build().await?);
        }

        written_files += 1;
        let progress = 1000 * written_files / count;
        if progress != last_progress && progress > 10 && progress < 1000 {
            // We already emitted ImexProgress(10) above
            context.emit_event(EventType::ImexProgress(progress));
            last_progress = progress;
        }
    }

    Ok(dir_builder)
}

/*******************************************************************************
 * Classic key import
 ******************************************************************************/
async fn import_self_keys(context: &Context, dir: &Path) -> Result<()> {
    /* hint: even if we switch to import Autocrypt Setup Files, we should leave the possibility to import
    plain ASC keys, at least keys without a password, if we do not want to implement a password entry function.
    Importing ASC keys is useful to use keys in Delta Chat used by any other non-Autocrypt-PGP implementation.

    Maybe we should make the "default" key handlong also a little bit smarter
    (currently, the last imported key is the standard key unless it contains the string "legacy" in its name) */
    let mut set_default: bool;
    let mut imported_cnt = 0;

    let dir_name = dir.to_string_lossy();
    let mut dir_handle = tokio::fs::read_dir(&dir).await?;
    while let Ok(Some(entry)) = dir_handle.next_entry().await {
        let entry_fn = entry.file_name();
        let name_f = entry_fn.to_string_lossy();
        let path_plus_name = dir.join(&entry_fn);
        match get_filesuffix_lc(&name_f) {
            Some(suffix) => {
                if suffix != "asc" {
                    continue;
                }
                set_default = if name_f.contains("legacy") {
                    info!(context, "found legacy key '{}'", path_plus_name.display());
                    false
                } else {
                    true
                }
            }
            None => {
                continue;
            }
        }
        info!(
            context,
            "considering key file: {}",
            path_plus_name.display()
        );

        match read_file(context, &path_plus_name).await {
            Ok(buf) => {
                let armored = std::string::String::from_utf8_lossy(&buf);
                if let Err(err) = set_self_key(context, &armored, set_default, false).await {
                    error!(context, "set_self_key: {}", err);
                    continue;
                }
            }
            Err(_) => continue,
        }
        imported_cnt += 1;
    }
    ensure!(
        imported_cnt > 0,
        "No private keys found in \"{}\".",
        dir_name
    );
    Ok(())
}

async fn export_self_keys(context: &Context, dir: &Path) -> Result<()> {
    let mut export_errors = 0;

    let keys = context
        .sql
        .query_map(
            "SELECT id, public_key, private_key, is_default FROM keypairs;",
            paramsv![],
            |row| {
                let id = row.get(0)?;
                let public_key_blob: Vec<u8> = row.get(1)?;
                let public_key = SignedPublicKey::from_slice(&public_key_blob);
                let private_key_blob: Vec<u8> = row.get(2)?;
                let private_key = SignedSecretKey::from_slice(&private_key_blob);
                let is_default: i32 = row.get(3)?;

                Ok((id, public_key, private_key, is_default))
            },
            |keys| {
                keys.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Into::into)
            },
        )
        .await?;

    for (id, public_key, private_key, is_default) in keys {
        let id = Some(id).filter(|_| is_default != 0);
        if let Ok(key) = public_key {
            if export_key_to_asc_file(context, dir, id, &key)
                .await
                .is_err()
            {
                export_errors += 1;
            }
        } else {
            export_errors += 1;
        }
        if let Ok(key) = private_key {
            if export_key_to_asc_file(context, dir, id, &key)
                .await
                .is_err()
            {
                export_errors += 1;
            }
        } else {
            export_errors += 1;
        }
    }

    ensure!(export_errors == 0, "errors while exporting keys");
    Ok(())
}

/*******************************************************************************
 * Classic key export
 ******************************************************************************/
async fn export_key_to_asc_file<T>(
    context: &Context,
    dir: &Path,
    id: Option<i64>,
    key: &T,
) -> std::io::Result<()>
where
    T: DcKey + Any,
{
    let file_name = {
        let any_key = key as &dyn Any;
        let kind = if any_key.downcast_ref::<SignedPublicKey>().is_some() {
            "public"
        } else if any_key.downcast_ref::<SignedSecretKey>().is_some() {
            "private"
        } else {
            "unknown"
        };
        let id = id.map_or("default".into(), |i| i.to_string());
        dir.join(format!("{}-key-{}.asc", kind, &id))
    };
    info!(
        context,
        "Exporting key {:?} to {}",
        key.key_id(),
        file_name.display()
    );
    delete_file(context, &file_name).await;

    let content = key.to_asc(None).into_bytes();
    let res = write_file(context, &file_name, &content).await;
    if res.is_err() {
        error!(context, "Cannot write key to {}", file_name.display());
    } else {
        context.emit_event(EventType::ImexFileWritten(file_name));
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pgp::{split_armored_data, HEADER_AUTOCRYPT, HEADER_SETUPCODE};
    use crate::stock_str::StockMessage;
    use crate::test_utils::{alice_keypair, TestContext};

    use ::pgp::armor::BlockType;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_render_setup_file() {
        let t = TestContext::new_alice().await;
        let msg = render_setup_file(&t, "hello").await.unwrap();
        println!("{}", &msg);
        // Check some substrings, indicating things got substituted.
        // In particular note the mixing of `\r\n` and `\n` depending
        // on who generated the stings.
        assert!(msg.contains("<title>Autocrypt Setup Message</title"));
        assert!(msg.contains("<h1>Autocrypt Setup Message</h1>"));
        assert!(msg.contains("<p>This is the Autocrypt Setup Message used to"));
        assert!(msg.contains("-----BEGIN PGP MESSAGE-----\r\n"));
        assert!(msg.contains("Passphrase-Format: numeric9x4\r\n"));
        assert!(msg.contains("Passphrase-Begin: he\n"));
        assert!(msg.contains("-----END PGP MESSAGE-----\n"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_render_setup_file_newline_replace() {
        let t = TestContext::new_alice().await;
        t.set_stock_translation(StockMessage::AcSetupMsgBody, "hello\r\nthere".to_string())
            .await
            .unwrap();
        let msg = render_setup_file(&t, "pw").await.unwrap();
        println!("{}", &msg);
        assert!(msg.contains("<p>hello<br>there</p>"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_create_setup_code() {
        let t = TestContext::new().await;
        let setupcode = create_setup_code(&t);
        assert_eq!(setupcode.len(), 44);
        assert_eq!(setupcode.chars().nth(4).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(9).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(14).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(19).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(24).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(29).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(34).unwrap(), '-');
        assert_eq!(setupcode.chars().nth(39).unwrap(), '-');
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_export_public_key_to_asc_file() {
        let context = TestContext::new().await;
        let key = alice_keypair().public;
        let blobdir = Path::new("$BLOBDIR");
        assert!(export_key_to_asc_file(&context.ctx, blobdir, None, &key)
            .await
            .is_ok());
        let blobdir = context.ctx.get_blobdir().to_str().unwrap();
        let filename = format!("{}/public-key-default.asc", blobdir);
        let bytes = tokio::fs::read(&filename).await.unwrap();

        assert_eq!(bytes, key.to_asc(None).into_bytes());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_export_private_key_to_asc_file() {
        let context = TestContext::new().await;
        let key = alice_keypair().secret;
        let blobdir = Path::new("$BLOBDIR");
        assert!(export_key_to_asc_file(&context.ctx, blobdir, None, &key)
            .await
            .is_ok());
        let blobdir = context.ctx.get_blobdir().to_str().unwrap();
        let filename = format!("{}/private-key-default.asc", blobdir);
        let bytes = tokio::fs::read(&filename).await.unwrap();

        assert_eq!(bytes, key.to_asc(None).into_bytes());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_export_and_import_key() {
        let context = TestContext::new_alice().await;
        let blobdir = context.ctx.get_blobdir();
        if let Err(err) = imex(&context.ctx, ImexMode::ExportSelfKeys, blobdir, None).await {
            panic!("got error on export: {:?}", err);
        }

        let context2 = TestContext::new_alice().await;
        if let Err(err) = imex(&context2.ctx, ImexMode::ImportSelfKeys, blobdir, None).await {
            panic!("got error on import: {:?}", err);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_export_and_import_backup() -> Result<()> {
        let backup_dir = tempfile::tempdir().unwrap();

        let context1 = TestContext::new_alice().await;
        assert!(context1.is_configured().await?);

        let context2 = TestContext::new().await;
        assert!(!context2.is_configured().await?);
        assert!(has_backup(&context2, backup_dir.path()).await.is_err());

        // export from context1
        assert!(
            imex(&context1, ImexMode::ExportBackup, backup_dir.path(), None)
                .await
                .is_ok()
        );
        let _event = context1
            .evtracker
            .get_matching(|evt| matches!(evt, EventType::ImexProgress(1000)))
            .await;

        // import to context2
        let backup = has_backup(&context2, backup_dir.path()).await?;

        // Import of unencrypted backup with incorrect "foobar" backup passphrase fails.
        assert!(imex(
            &context2,
            ImexMode::ImportBackup,
            backup.as_ref(),
            Some("foobar".to_string())
        )
        .await
        .is_err());

        assert!(
            imex(&context2, ImexMode::ImportBackup, backup.as_ref(), None)
                .await
                .is_ok()
        );
        let _event = context2
            .evtracker
            .get_matching(|evt| matches!(evt, EventType::ImexProgress(1000)))
            .await;

        assert!(context2.is_configured().await?);
        assert_eq!(
            context2.get_config(Config::Addr).await?,
            Some("alice@example.org".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_normalize_setup_code() {
        let norm = normalize_setup_code("123422343234423452346234723482349234");
        assert_eq!(norm, "1234-2234-3234-4234-5234-6234-7234-8234-9234");

        let norm =
            normalize_setup_code("\t1 2 3422343234- foo bar-- 423-45 2 34 6234723482349234      ");
        assert_eq!(norm, "1234-2234-3234-4234-5234-6234-7234-8234-9234");
    }

    /* S_EM_SETUPFILE is a AES-256 symm. encrypted setup message created by Enigmail
    with an "encrypted session key", see RFC 4880.  The code is in S_EM_SETUPCODE */
    const S_EM_SETUPCODE: &str = "1742-0185-6197-1303-7016-8412-3581-4441-0597";
    const S_EM_SETUPFILE: &str = include_str!("../test-data/message/stress.txt");

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_split_and_decrypt() {
        let buf_1 = S_EM_SETUPFILE.as_bytes().to_vec();
        let (typ, headers, base64) = split_armored_data(&buf_1).unwrap();
        assert_eq!(typ, BlockType::Message);
        assert!(S_EM_SETUPCODE.starts_with(headers.get(HEADER_SETUPCODE).unwrap()));
        assert!(headers.get(HEADER_AUTOCRYPT).is_none());

        assert!(!base64.is_empty());

        let setup_file = S_EM_SETUPFILE.to_string();
        let decrypted =
            decrypt_setup_file(S_EM_SETUPCODE, std::io::Cursor::new(setup_file.as_bytes()))
                .await
                .unwrap();

        let (typ, headers, _base64) = split_armored_data(decrypted.as_bytes()).unwrap();

        assert_eq!(typ, BlockType::PrivateKey);
        assert_eq!(headers.get(HEADER_AUTOCRYPT), Some(&"mutual".to_string()));
        assert!(headers.get(HEADER_SETUPCODE).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_key_transfer() -> Result<()> {
        let alice = TestContext::new_alice().await;

        let alice_clone = alice.clone();
        let key_transfer_task = tokio::task::spawn(async move {
            let ctx = alice_clone;
            initiate_key_transfer(&ctx).await
        });

        // Wait for the message to be added to the queue.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let sent = alice.pop_sent_msg().await;
        let setup_code = key_transfer_task.await??;

        // Alice sets up a second device.
        let alice2 = TestContext::new().await;
        alice2.set_name("alice2");
        alice2.configure_addr("alice@example.org").await;
        alice2.recv_msg(&sent).await;
        let msg = alice2.get_last_msg().await;

        // Send a message that cannot be decrypted because the keys are
        // not synchronized yet.
        let sent = alice2.send_text(msg.chat_id, "Test").await;
        alice.recv_msg(&sent).await;
        assert_ne!(
            alice.get_last_msg().await.get_text(),
            Some("Test".to_string())
        );

        // Transfer the key.
        continue_key_transfer(&alice2, msg.id, &setup_code).await?;

        // Alice sends a message to self from the new device.
        let sent = alice2.send_text(msg.chat_id, "Test").await;
        alice.recv_msg(&sent).await;
        assert_eq!(
            alice.get_last_msg().await.get_text(),
            Some("Test".to_string())
        );

        Ok(())
    }
}
