//! Internet Message Format reception pipeline.

use std::cmp::min;
use std::collections::HashSet;
use std::convert::TryFrom;

use anyhow::{bail, ensure, Context as _, Result};
use mailparse::{parse_mail, SingleInfo};
use num_traits::FromPrimitive;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::chat::{self, Chat, ChatId, ChatIdBlocked, ProtectionStatus};
use crate::config::Config;
use crate::constants::{Blocked, Chattype, ShowEmails, DC_CHAT_ID_TRASH};
use crate::contact;
use crate::contact::{
    may_be_valid_addr, normalize_name, Contact, ContactId, Origin, VerifiedStatus,
};
use crate::context::Context;
use crate::download::DownloadState;
use crate::ephemeral::{stock_ephemeral_timer_changed, Timer as EphemeralTimer};
use crate::events::EventType;
use crate::headerdef::{HeaderDef, HeaderDefMap};
use crate::imap::markseen_on_imap_table;
use crate::location;
use crate::log::LogExt;
use crate::message::{
    self, rfc724_mid_exists, Message, MessageState, MessengerMessage, MsgId, Viewtype,
};
use crate::mimeparser::{
    parse_message_id, parse_message_ids, AvatarAction, MailinglistType, MimeMessage, SystemMessage,
};
use crate::param::{Param, Params};
use crate::peerstate::{Peerstate, PeerstateKeyType, PeerstateVerifiedStatus};
use crate::securejoin::{self, handle_securejoin_handshake, observe_securejoin_on_other_device};
use crate::sql;
use crate::stock_str;
use crate::tools::{create_id, extract_grpid_from_rfc724_mid, smeared_time};

/// This is the struct that is returned after receiving one email (aka MIME message).
///
/// One email with multiple attachments can end up as multiple chat messages, but they
/// all have the same chat_id, state and sort_timestamp.
#[derive(Debug)]
pub struct ReceivedMsg {
    pub chat_id: ChatId,
    pub state: MessageState,
    pub sort_timestamp: i64,

    /// IDs of inserted rows in messages table.
    pub msg_ids: Vec<MsgId>,

    /// Whether IMAP messages should be immediately deleted.
    pub needs_delete_job: bool,
}

/// Emulates reception of a message from the network.
///
/// This method returns errors on a failure to parse the mail or extract Message-ID. It's only used
/// for tests and REPL tool, not actual message reception pipeline.
pub async fn receive_imf(
    context: &Context,
    imf_raw: &[u8],
    seen: bool,
) -> Result<Option<ReceivedMsg>> {
    let mail = parse_mail(imf_raw).context("can't parse mail")?;
    let rfc724_mid = mail
        .headers
        .get_header_value(HeaderDef::MessageId)
        .and_then(|msgid| parse_message_id(&msgid).ok())
        .unwrap_or_else(create_id);
    receive_imf_inner(context, &rfc724_mid, imf_raw, seen, None, false).await
}

/// Receive a message and add it to the database.
///
/// Returns an error on recoverable errors, e.g. database errors. In this case,
/// message parsing should be retried later.
///
/// If message itself is wrong, logs
/// the error and returns success:
/// - If possible, creates a database entry to prevent the message from being
///   downloaded again, sets `chat_id=DC_CHAT_ID_TRASH` and returns `Ok(Some(…))`
/// - If the message is so wrong that we didn't even create a database entry,
///   returns `Ok(None)`
///
/// If `is_partial_download` is set, it contains the full message size in bytes.
/// Do not confuse that with `replace_partial_download` that will be set when the full message is loaded later.
pub(crate) async fn receive_imf_inner(
    context: &Context,
    rfc724_mid: &str,
    imf_raw: &[u8],
    seen: bool,
    is_partial_download: Option<u32>,
    fetching_existing_messages: bool,
) -> Result<Option<ReceivedMsg>> {
    info!(context, "Receiving message, seen={}...", seen);

    if std::env::var(crate::DCC_MIME_DEBUG).unwrap_or_default() == "2" {
        info!(context, "receive_imf: incoming message mime-body:");
        println!("{}", String::from_utf8_lossy(imf_raw));
    }

    let mut mime_parser =
        match MimeMessage::from_bytes_with_partial(context, imf_raw, is_partial_download).await {
            Err(err) => {
                warn!(context, "receive_imf: can't parse MIME: {}", err);
                return Ok(None);
            }
            Ok(mime_parser) => mime_parser,
        };

    // we can not add even an empty record if we have no info whatsoever
    if !mime_parser.has_headers() {
        warn!(context, "receive_imf: no headers found");
        return Ok(None);
    }

    info!(context, "received message has Message-Id: {}", rfc724_mid);

    // check, if the mail is already in our database.
    // make sure, this check is done eg. before securejoin-processing.
    let replace_partial_download =
        if let Some(old_msg_id) = message::rfc724_mid_exists(context, rfc724_mid).await? {
            let msg = Message::load_from_db(context, old_msg_id).await?;
            if msg.download_state() != DownloadState::Done && is_partial_download.is_none() {
                // the mesage was partially downloaded before and is fully downloaded now.
                info!(
                    context,
                    "Message already partly in DB, replacing by full message."
                );
                Some(old_msg_id)
            } else {
                // the message was probably moved around.
                info!(context, "Message already in DB, doing nothing.");
                return Ok(None);
            }
        } else {
            None
        };

    // the function returns the number of created messages in the database
    let prevent_rename =
        mime_parser.is_mailinglist_message() || mime_parser.get_header(HeaderDef::Sender).is_some();

    // get From: (it can be an address list!) and check if it is known (for known From:'s we add
    // the other To:/Cc: in the 3rd pass)
    // or if From: is equal to SELF (in this case, it is any outgoing messages,
    // we do not check Return-Path any more as this is unreliable, see
    // <https://github.com/deltachat/deltachat-core/issues/150>)
    //
    // If this is a mailing list email (i.e. list_id_header is some), don't change the displayname because in
    // a mailing list the sender displayname sometimes does not belong to the sender email address.
    let (from_id, _from_id_blocked, incoming_origin) =
        from_field_to_contact_id(context, &mime_parser.from, prevent_rename).await?;

    let incoming = from_id != ContactId::SELF;

    let to_ids = add_or_lookup_contacts_by_address_list(
        context,
        &mime_parser.recipients,
        if !incoming {
            Origin::OutgoingTo
        } else if incoming_origin.is_known() {
            Origin::IncomingTo
        } else {
            Origin::IncomingUnknownTo
        },
        prevent_rename,
    )
    .await?;

    let rcvd_timestamp = smeared_time(context).await;
    let sent_timestamp = mime_parser
        .get_header(HeaderDef::Date)
        .and_then(|value| mailparse::dateparse(value).ok())
        .map_or(rcvd_timestamp, |value| min(value, rcvd_timestamp));

    // Add parts
    let received_msg = add_parts(
        context,
        &mut mime_parser,
        imf_raw,
        incoming,
        &to_ids,
        rfc724_mid,
        sent_timestamp,
        rcvd_timestamp,
        from_id,
        seen || replace_partial_download.is_some(),
        is_partial_download,
        replace_partial_download,
        fetching_existing_messages,
        prevent_rename,
    )
    .await
    .context("add_parts error")?;

    if !from_id.is_special() {
        contact::update_last_seen(context, from_id, sent_timestamp).await?;
    }

    // Update gossiped timestamp for the chat if someone else or our other device sent
    // Autocrypt-Gossip for all recipients in the chat to avoid sending Autocrypt-Gossip ourselves
    // and waste traffic.
    let chat_id = received_msg.chat_id;
    if !chat_id.is_special()
        && mime_parser
            .recipients
            .iter()
            .all(|recipient| mime_parser.gossiped_addr.contains(&recipient.addr))
    {
        info!(
            context,
            "Received message contains Autocrypt-Gossip for all members, updating timestamp."
        );
        if chat_id.get_gossiped_timestamp(context).await? < sent_timestamp {
            chat_id
                .set_gossiped_timestamp(context, sent_timestamp)
                .await?;
        }
    }

    let insert_msg_id = if let Some(msg_id) = received_msg.msg_ids.last() {
        *msg_id
    } else {
        MsgId::new_unset()
    };

    save_locations(context, &mime_parser, chat_id, from_id, insert_msg_id).await?;

    if let Some(ref sync_items) = mime_parser.sync_items {
        if from_id == ContactId::SELF {
            if mime_parser.was_encrypted() {
                if let Err(err) = context.execute_sync_items(sync_items).await {
                    warn!(context, "receive_imf cannot execute sync items: {}", err);
                }
            } else {
                warn!(context, "sync items are not encrypted.");
            }
        } else {
            warn!(context, "sync items not sent by self.");
        }
    }

    if let Some(ref status_update) = mime_parser.webxdc_status_update {
        if let Err(err) = context
            .receive_status_update(from_id, insert_msg_id, status_update)
            .await
        {
            warn!(context, "receive_imf cannot update status: {}", err);
        }
    }

    if let Some(avatar_action) = &mime_parser.user_avatar {
        if from_id != ContactId::UNDEFINED
            && context
                .update_contacts_timestamp(from_id, Param::AvatarTimestamp, sent_timestamp)
                .await?
        {
            match contact::set_profile_image(
                context,
                from_id,
                avatar_action,
                mime_parser.was_encrypted(),
            )
            .await
            {
                Ok(()) => {
                    context.emit_event(EventType::ChatModified(chat_id));
                }
                Err(err) => {
                    warn!(context, "receive_imf cannot update profile image: {}", err);
                }
            };
        }
    }

    // Always update the status, even if there is no footer, to allow removing the status.
    //
    // Ignore MDNs though, as they never contain the signature even if user has set it.
    // Ignore footers from mailinglists as they are often created or modified by the mailinglist software.
    if mime_parser.mdn_reports.is_empty()
        && !mime_parser.is_mailinglist_message()
        && is_partial_download.is_none()
        && from_id != ContactId::UNDEFINED
        && context
            .update_contacts_timestamp(from_id, Param::StatusTimestamp, sent_timestamp)
            .await?
    {
        if let Err(err) = contact::set_status(
            context,
            from_id,
            mime_parser.footer.clone().unwrap_or_default(),
            mime_parser.was_encrypted(),
            mime_parser.has_chat_version(),
        )
        .await
        {
            warn!(context, "cannot update contact status: {}", err);
        }
    }

    // Get user-configured server deletion
    let delete_server_after = context.get_config_delete_server_after().await?;

    if !received_msg.msg_ids.is_empty() {
        if received_msg.needs_delete_job
            || (delete_server_after == Some(0) && is_partial_download.is_none())
        {
            context
                .sql
                .execute(
                    "UPDATE imap SET target='' WHERE rfc724_mid=?",
                    paramsv![rfc724_mid],
                )
                .await?;
        } else if !mime_parser.mdn_reports.is_empty() && mime_parser.has_chat_version() {
            // This is a Delta Chat MDN. Mark as read.
            markseen_on_imap_table(context, rfc724_mid).await?;
        }
    }

    if replace_partial_download.is_some() {
        context.emit_msgs_changed(chat_id, MsgId::new(0));
    } else if !chat_id.is_trash() {
        let fresh = received_msg.state == MessageState::InFresh;
        for msg_id in &received_msg.msg_ids {
            if incoming && fresh {
                context.emit_incoming_msg(chat_id, *msg_id);
            } else {
                context.emit_msgs_changed(chat_id, *msg_id);
            };
        }
    }

    mime_parser
        .handle_reports(context, from_id, sent_timestamp, &mime_parser.parts)
        .await;

    Ok(Some(received_msg))
}

/// Converts "From" field to contact id.
///
/// Also returns whether it is blocked or not and its origin.
///
/// * `prevent_rename`: passed through to `add_or_lookup_contacts_by_address_list()`
pub async fn from_field_to_contact_id(
    context: &Context,
    from_address_list: &[SingleInfo],
    prevent_rename: bool,
) -> Result<(ContactId, bool, Origin)> {
    let from_ids = add_or_lookup_contacts_by_address_list(
        context,
        from_address_list,
        Origin::IncomingUnknownFrom,
        prevent_rename,
    )
    .await?;

    if from_ids.contains(&ContactId::SELF) {
        Ok((ContactId::SELF, false, Origin::OutgoingBcc))
    } else if !from_ids.is_empty() {
        if from_ids.len() > 1 {
            warn!(
                context,
                "mail has more than one From address, only using first: {:?}", from_address_list
            );
        }
        let from_id = from_ids.get(0).cloned().unwrap_or_default();

        let mut from_id_blocked = false;
        let mut incoming_origin = Origin::Unknown;
        if let Ok(contact) = Contact::load_from_db(context, from_id).await {
            from_id_blocked = contact.blocked;
            incoming_origin = contact.origin;
        }
        Ok((from_id, from_id_blocked, incoming_origin))
    } else {
        warn!(
            context,
            "mail has an empty From header: {:?}", from_address_list
        );

        Ok((ContactId::UNDEFINED, false, Origin::Unknown))
    }
}

#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
async fn add_parts(
    context: &Context,
    mime_parser: &mut MimeMessage,
    imf_raw: &[u8],
    incoming: bool,
    to_ids: &[ContactId],
    rfc724_mid: &str,
    sent_timestamp: i64,
    rcvd_timestamp: i64,
    from_id: ContactId,
    seen: bool,
    is_partial_download: Option<u32>,
    replace_msg_id: Option<MsgId>,
    fetching_existing_messages: bool,
    prevent_rename: bool,
) -> Result<ReceivedMsg> {
    let mut chat_id = None;
    let mut chat_id_blocked = Blocked::Not;

    let mut better_msg = None;
    if mime_parser.is_system_message == SystemMessage::LocationStreamingEnabled {
        better_msg = Some(stock_str::msg_location_enabled_by(context, from_id).await);
    }

    let parent = get_parent_message(context, mime_parser).await?;

    let is_dc_message = if mime_parser.has_chat_version() {
        MessengerMessage::Yes
    } else if let Some(parent) = &parent {
        match parent.is_dc_message {
            MessengerMessage::No => MessengerMessage::No,
            MessengerMessage::Yes | MessengerMessage::Reply => MessengerMessage::Reply,
        }
    } else {
        MessengerMessage::No
    };
    // incoming non-chat messages may be discarded

    let location_kml_is = mime_parser.location_kml.is_some();
    let is_mdn = !mime_parser.mdn_reports.is_empty();
    let show_emails =
        ShowEmails::from_i32(context.get_config_int(Config::ShowEmails).await?).unwrap_or_default();

    let allow_creation;
    if mime_parser.is_system_message != SystemMessage::AutocryptSetupMessage
        && is_dc_message == MessengerMessage::No
    {
        // this message is a classic email not a chat-message nor a reply to one
        match show_emails {
            ShowEmails::Off => {
                info!(context, "Classical email not shown (TRASH)");
                chat_id = Some(DC_CHAT_ID_TRASH);
                allow_creation = false;
            }
            ShowEmails::AcceptedContacts => allow_creation = false,
            ShowEmails::All => allow_creation = !is_mdn,
        }
    } else {
        allow_creation = !is_mdn;
    }

    // check if the message introduces a new chat:
    // - outgoing messages introduce a chat with the first to: address if they are sent by a messenger
    // - incoming messages introduce a chat only for known contacts if they are sent by a messenger
    // (of course, the user can add other chats manually later)
    let to_id: ContactId;

    let state: MessageState;
    let mut needs_delete_job = false;
    if incoming {
        to_id = ContactId::SELF;

        // Whether the message is a part of securejoin handshake that should be marked as seen
        // automatically.
        let securejoin_seen;

        // handshake may mark contacts as verified and must be processed before chats are created
        if mime_parser.get_header(HeaderDef::SecureJoin).is_some() {
            match handle_securejoin_handshake(context, mime_parser, from_id).await {
                Ok(securejoin::HandshakeMessage::Done) => {
                    chat_id = Some(DC_CHAT_ID_TRASH);
                    needs_delete_job = true;
                    securejoin_seen = true;
                }
                Ok(securejoin::HandshakeMessage::Ignore) => {
                    chat_id = Some(DC_CHAT_ID_TRASH);
                    securejoin_seen = true;
                }
                Ok(securejoin::HandshakeMessage::Propagate) => {
                    // process messages as "member added" normally
                    securejoin_seen = false;
                }
                Err(err) => {
                    warn!(context, "Error in Secure-Join message handling: {}", err);
                    chat_id = Some(DC_CHAT_ID_TRASH);
                    securejoin_seen = true;
                }
            }
        } else {
            securejoin_seen = false;
        }

        let test_normal_chat = if from_id == ContactId::UNDEFINED {
            Default::default()
        } else {
            ChatIdBlocked::lookup_by_contact(context, from_id).await?
        };

        if chat_id.is_none() && mime_parser.delivery_report.is_some() {
            chat_id = Some(DC_CHAT_ID_TRASH);
            info!(context, "Message is a DSN (TRASH)",);
        }

        if chat_id.is_none() {
            // try to assign to a chat based on In-Reply-To/References:

            if let Some((new_chat_id, new_chat_id_blocked)) =
                lookup_chat_by_reply(context, mime_parser, &parent, to_ids, from_id).await?
            {
                chat_id = Some(new_chat_id);
                chat_id_blocked = new_chat_id_blocked;
            }
        }

        if chat_id.is_none() {
            // try to create a group

            let create_blocked = match test_normal_chat {
                Some(ChatIdBlocked {
                    id: _,
                    blocked: Blocked::Not,
                }) => Blocked::Not,
                _ => Blocked::Request,
            };

            if let Some((new_chat_id, new_chat_id_blocked)) = create_or_lookup_group(
                context,
                mime_parser,
                if test_normal_chat.is_none() {
                    allow_creation
                } else {
                    true
                },
                create_blocked,
                from_id,
                to_ids,
            )
            .await?
            {
                chat_id = Some(new_chat_id);
                chat_id_blocked = new_chat_id_blocked;
                if chat_id_blocked != Blocked::Not && create_blocked == Blocked::Not {
                    new_chat_id.unblock(context).await?;
                    chat_id_blocked = Blocked::Not;
                }
            }
        }

        // In lookup_chat_by_reply() and create_or_lookup_group(), it can happen that the message is put into a chat
        // but the From-address is not a member of this chat.
        if let Some(chat_id) = chat_id {
            if !chat::is_contact_in_chat(context, chat_id, from_id).await? {
                let chat = Chat::load_from_db(context, chat_id).await?;
                if chat.is_protected() {
                    let s = stock_str::unknown_sender_for_chat(context).await;
                    mime_parser.repl_msg_by_error(&s);
                } else if let Some(from) = mime_parser.from.first() {
                    // In non-protected chats, just mark the sender as overridden. Therefore, the UI will prepend `~`
                    // to the sender's name, indicating to the user that he/she is not part of the group.
                    let name: &str = from.display_name.as_ref().unwrap_or(&from.addr);
                    for part in mime_parser.parts.iter_mut() {
                        part.param.set(Param::OverrideSenderDisplayname, name);
                    }
                }
            }

            better_msg = better_msg.or(apply_group_changes(
                context,
                mime_parser,
                sent_timestamp,
                chat_id,
                from_id,
                to_ids,
            )
            .await?);
        }

        if chat_id.is_none() {
            // check if the message belongs to a mailing list
            match mime_parser.get_mailinglist_type() {
                MailinglistType::ListIdBased => {
                    if let Some(list_id) = mime_parser.get_header(HeaderDef::ListId) {
                        if let Some((new_chat_id, new_chat_id_blocked)) =
                            create_or_lookup_mailinglist(
                                context,
                                allow_creation,
                                list_id,
                                mime_parser,
                            )
                            .await?
                        {
                            chat_id = Some(new_chat_id);
                            chat_id_blocked = new_chat_id_blocked;
                        }
                    }
                }
                MailinglistType::SenderBased => {
                    if let Some(sender) = mime_parser.get_header(HeaderDef::Sender) {
                        if let Some((new_chat_id, new_chat_id_blocked)) =
                            create_or_lookup_mailinglist(
                                context,
                                allow_creation,
                                sender,
                                mime_parser,
                            )
                            .await?
                        {
                            chat_id = Some(new_chat_id);
                            chat_id_blocked = new_chat_id_blocked;
                        }
                    }
                }
                MailinglistType::None => {}
            }
        }

        if let Some(chat_id) = chat_id {
            apply_mailinglist_changes(context, mime_parser, chat_id).await?;
        }

        // if contact renaming is prevented (for mailinglists and bots),
        // we use name from From:-header as override name
        if prevent_rename {
            if let Some(from) = mime_parser.from.first() {
                if let Some(name) = &from.display_name {
                    for part in mime_parser.parts.iter_mut() {
                        part.param.set(Param::OverrideSenderDisplayname, name);
                    }
                }
            }
        }

        if chat_id.is_none() {
            // try to create a normal chat
            let create_blocked = if from_id == ContactId::SELF {
                Blocked::Not
            } else {
                let contact = Contact::load_from_db(context, from_id).await?;
                if contact.is_blocked() {
                    Blocked::Yes
                } else {
                    Blocked::Request
                }
            };

            if let Some(chat) = test_normal_chat {
                chat_id = Some(chat.id);
                chat_id_blocked = chat.blocked;
            } else if allow_creation {
                if let Ok(chat) = ChatIdBlocked::get_for_contact(context, from_id, create_blocked)
                    .await
                    .log_err(context, "Failed to get (new) chat for contact")
                {
                    chat_id = Some(chat.id);
                    chat_id_blocked = chat.blocked;
                }
            }

            if let Some(chat_id) = chat_id {
                if chat_id_blocked != Blocked::Not {
                    if chat_id_blocked != create_blocked {
                        chat_id.set_blocked(context, create_blocked).await?;
                    }
                    if create_blocked == Blocked::Request && parent.is_some() {
                        // we do not want any chat to be created implicitly.  Because of the origin-scale-up,
                        // the contact requests will pop up and this should be just fine.
                        Contact::scaleup_origin_by_id(context, from_id, Origin::IncomingReplyTo)
                            .await?;
                        info!(
                            context,
                            "Message is a reply to a known message, mark sender as known.",
                        );
                    }
                }
            }
        }

        state =
            if seen || fetching_existing_messages || is_mdn || location_kml_is || securejoin_seen {
                MessageState::InSeen
            } else {
                MessageState::InFresh
            };
    } else {
        // Outgoing

        // the mail is on the IMAP server, probably it is also delivered.
        // We cannot recreate other states (read, error).
        state = MessageState::OutDelivered;
        to_id = to_ids.get(0).cloned().unwrap_or_default();

        let self_sent =
            from_id == ContactId::SELF && to_ids.len() == 1 && to_ids.contains(&ContactId::SELF);

        // handshake may mark contacts as verified and must be processed before chats are created
        if mime_parser.get_header(HeaderDef::SecureJoin).is_some() {
            match observe_securejoin_on_other_device(context, mime_parser, to_id).await {
                Ok(securejoin::HandshakeMessage::Done)
                | Ok(securejoin::HandshakeMessage::Ignore) => {
                    chat_id = Some(DC_CHAT_ID_TRASH);
                }
                Ok(securejoin::HandshakeMessage::Propagate) => {
                    // process messages as "member added" normally
                    chat_id = None;
                }
                Err(err) => {
                    warn!(context, "Error in Secure-Join watching: {}", err);
                    chat_id = Some(DC_CHAT_ID_TRASH);
                }
            }
        } else if mime_parser.sync_items.is_some() && self_sent {
            chat_id = Some(DC_CHAT_ID_TRASH);
        }

        // Mozilla Thunderbird does not set \Draft flag on "Templates", but sets
        // X-Mozilla-Draft-Info header, which can be used to detect both drafts and templates
        // created by Thunderbird.
        let is_draft = mime_parser
            .get_header(HeaderDef::XMozillaDraftInfo)
            .is_some();

        if is_draft {
            // Most mailboxes have a "Drafts" folder where constantly new emails appear but we don't actually want to show them
            info!(context, "Email is probably just a draft (TRASH)");
            chat_id = Some(DC_CHAT_ID_TRASH);
        }

        if chat_id.is_none() {
            // try to assign to a chat based on In-Reply-To/References:

            if let Some((new_chat_id, new_chat_id_blocked)) =
                lookup_chat_by_reply(context, mime_parser, &parent, to_ids, from_id).await?
            {
                chat_id = Some(new_chat_id);
                chat_id_blocked = new_chat_id_blocked;
            }
        }

        if !to_ids.is_empty() {
            if chat_id.is_none() {
                if let Some((new_chat_id, new_chat_id_blocked)) = create_or_lookup_group(
                    context,
                    mime_parser,
                    allow_creation,
                    Blocked::Not,
                    from_id,
                    to_ids,
                )
                .await?
                {
                    chat_id = Some(new_chat_id);
                    chat_id_blocked = new_chat_id_blocked;
                }
            }
            if chat_id.is_none() && allow_creation {
                let to_contact = Contact::load_from_db(context, to_id).await?;
                if let Some(list_id) = to_contact.param.get(Param::ListId) {
                    if let Some((id, _, blocked)) =
                        chat::get_chat_id_by_grpid(context, list_id).await?
                    {
                        chat_id = Some(id);
                        chat_id_blocked = blocked;
                    }
                } else if let Ok(chat) =
                    ChatIdBlocked::get_for_contact(context, to_id, Blocked::Not).await
                {
                    chat_id = Some(chat.id);
                    chat_id_blocked = chat.blocked;
                }
            }

            // automatically unblock chat when the user sends a message
            if chat_id_blocked != Blocked::Not {
                if let Some(chat_id) = chat_id {
                    chat_id.unblock(context).await?;
                    chat_id_blocked = Blocked::Not;
                }
            }
        }

        if let Some(chat_id) = chat_id {
            better_msg = better_msg.or(apply_group_changes(
                context,
                mime_parser,
                sent_timestamp,
                chat_id,
                from_id,
                to_ids,
            )
            .await?);
        }

        if chat_id.is_none() && self_sent {
            // from_id==to_id==ContactId::SELF - this is a self-sent messages,
            // maybe an Autocrypt Setup Message
            if let Ok(chat) = ChatIdBlocked::get_for_contact(context, ContactId::SELF, Blocked::Not)
                .await
                .log_err(context, "Failed to get (new) chat for contact")
            {
                chat_id = Some(chat.id);
                chat_id_blocked = chat.blocked;
            }

            if let Some(chat_id) = chat_id {
                if Blocked::Not != chat_id_blocked {
                    chat_id.unblock(context).await?;
                    // Not assigning `chat_id_blocked = Blocked::Not` to avoid unused_assignments warning.
                }
            }
        }
    }

    if fetching_existing_messages && mime_parser.decrypting_failed {
        chat_id = Some(DC_CHAT_ID_TRASH);
        // We are only gathering old messages on first start. We do not want to add loads of non-decryptable messages to the chats.
        info!(context, "Existing non-decipherable message. (TRASH)");
    }

    if mime_parser.webxdc_status_update.is_some() && mime_parser.parts.len() == 1 {
        if let Some(part) = mime_parser.parts.first() {
            if part.typ == Viewtype::Text && part.msg.is_empty() {
                chat_id = Some(DC_CHAT_ID_TRASH);
                info!(context, "Message is a status update only (TRASH)");
            }
        }
    }

    if is_mdn {
        chat_id = Some(DC_CHAT_ID_TRASH);
    }

    let chat_id = chat_id.unwrap_or_else(|| {
        info!(context, "No chat id for message (TRASH)");
        DC_CHAT_ID_TRASH
    });

    // Extract ephemeral timer from the message or use the existing timer if the message is not fully downloaded.
    let mut ephemeral_timer = if is_partial_download.is_some() {
        chat_id.get_ephemeral_timer(context).await?
    } else if let Some(value) = mime_parser.get_header(HeaderDef::EphemeralTimer) {
        match value.parse::<EphemeralTimer>() {
            Ok(timer) => timer,
            Err(err) => {
                warn!(
                    context,
                    "can't parse ephemeral timer \"{}\": {}", value, err
                );
                EphemeralTimer::Disabled
            }
        }
    } else {
        EphemeralTimer::Disabled
    };

    let in_fresh = state == MessageState::InFresh;
    let sort_timestamp = calc_sort_timestamp(context, sent_timestamp, chat_id, in_fresh).await?;

    // Apply ephemeral timer changes to the chat.
    //
    // Only apply the timer when there are visible parts (e.g., the message does not consist only
    // of `location.kml` attachment).  Timer changes without visible received messages may be
    // confusing to the user.
    if !chat_id.is_special()
        && !mime_parser.parts.is_empty()
        && chat_id.get_ephemeral_timer(context).await? != ephemeral_timer
    {
        info!(
            context,
            "received new ephemeral timer value {:?} for chat {}, checking if it should be applied",
            ephemeral_timer,
            chat_id
        );
        if is_dc_message == MessengerMessage::Yes
            && get_previous_message(context, mime_parser)
                .await?
                .map(|p| p.ephemeral_timer)
                == Some(ephemeral_timer)
            && mime_parser.is_system_message != SystemMessage::EphemeralTimerChanged
        {
            // The message is a Delta Chat message, so we know that previous message according to
            // References header is the last message in the chat as seen by the sender. The timer
            // is the same in both the received message and the last message, so we know that the
            // sender has not seen any change of the timer between these messages. As our timer
            // value is different, it means the sender has not received some timer update that we
            // have seen or sent ourselves, so we ignore incoming timer to prevent a rollback.
            warn!(
                context,
                "ignoring ephemeral timer change to {:?} for chat {} to avoid rollback",
                ephemeral_timer,
                chat_id
            );
        } else if chat_id
            .update_timestamp(context, Param::EphemeralSettingsTimestamp, sent_timestamp)
            .await?
        {
            if let Err(err) = chat_id
                .inner_set_ephemeral_timer(context, ephemeral_timer)
                .await
            {
                warn!(
                    context,
                    "failed to modify timer for chat {}: {}", chat_id, err
                );
            } else {
                info!(
                    context,
                    "updated ephemeral timer to {:?} for chat {}", ephemeral_timer, chat_id
                );
                if mime_parser.is_system_message != SystemMessage::EphemeralTimerChanged {
                    chat::add_info_msg(
                        context,
                        chat_id,
                        &stock_ephemeral_timer_changed(context, ephemeral_timer, from_id).await,
                        sort_timestamp,
                    )
                    .await?;
                }
            }
        } else {
            warn!(
                context,
                "ignoring ephemeral timer change to {:?} because it's outdated", ephemeral_timer
            );
        }
    }

    if mime_parser.is_system_message == SystemMessage::EphemeralTimerChanged {
        better_msg = Some(stock_ephemeral_timer_changed(context, ephemeral_timer, from_id).await);

        // Do not delete the system message itself.
        //
        // This prevents confusion when timer is changed
        // to 1 week, and then changed to 1 hour: after 1
        // hour, only the message about the change to 1
        // week is left.
        ephemeral_timer = EphemeralTimer::Disabled;
    }

    // if a chat is protected and the message is fully downloaded, check additional properties
    if !chat_id.is_special() && is_partial_download.is_none() {
        let chat = Chat::load_from_db(context, chat_id).await?;
        let new_status = match mime_parser.is_system_message {
            SystemMessage::ChatProtectionEnabled => Some(ProtectionStatus::Protected),
            SystemMessage::ChatProtectionDisabled => Some(ProtectionStatus::Unprotected),
            _ => None,
        };

        if chat.is_protected() || new_status.is_some() {
            if let Err(err) = check_verified_properties(context, mime_parser, from_id, to_ids).await
            {
                warn!(context, "verification problem: {}", err);
                let s = format!("{}. See 'Info' for more details", err);
                mime_parser.repl_msg_by_error(&s);
            } else {
                // change chat protection only when verification check passes
                if let Some(new_status) = new_status {
                    if chat_id
                        .update_timestamp(
                            context,
                            Param::ProtectionSettingsTimestamp,
                            sent_timestamp,
                        )
                        .await?
                    {
                        if let Err(e) = chat_id.inner_set_protection(context, new_status).await {
                            chat::add_info_msg(
                                context,
                                chat_id,
                                &format!("Cannot set protection: {}", e),
                                sort_timestamp,
                            )
                            .await?;
                            // do not return an error as this would result in retrying the message
                        }
                    }
                    better_msg = Some(context.stock_protection_msg(new_status, from_id).await);
                }
            }
        }
    }

    // Ensure replies to messages are sorted after the parent message.
    //
    // This is useful in a case where sender clocks are not
    // synchronized and parent message has a Date: header with a
    // timestamp higher than reply timestamp.
    //
    // This does not help if parent message arrives later than the
    // reply.
    let parent_timestamp = mime_parser.get_parent_timestamp(context).await?;
    let sort_timestamp = parent_timestamp.map_or(sort_timestamp, |parent_timestamp| {
        std::cmp::max(sort_timestamp, parent_timestamp)
    });

    // if the mime-headers should be saved, find out its size
    // (the mime-header ends with an empty line)
    let save_mime_headers = context.get_config_bool(Config::SaveMimeHeaders).await?;

    let mime_in_reply_to = mime_parser
        .get_header(HeaderDef::InReplyTo)
        .cloned()
        .unwrap_or_default();
    let mime_references = mime_parser
        .get_header(HeaderDef::References)
        .cloned()
        .unwrap_or_default();

    // fine, so far.  now, split the message into simple parts usable as "short messages"
    // and add them to the database (mails sent by other messenger clients should result
    // into only one message; mails sent by other clients may result in several messages
    // (eg. one per attachment))
    let icnt = mime_parser.parts.len();

    let subject = mime_parser.get_subject().unwrap_or_default();

    let is_system_message = mime_parser.is_system_message;

    // if indicated by the parser,
    // we save the full mime-message and add a flag
    // that the ui should show button to display the full message.

    // a flag used to avoid adding "show full message" button to multiple parts of the message.
    let mut save_mime_modified = mime_parser.is_mime_modified;

    let mime_headers = if save_mime_headers || save_mime_modified {
        if mime_parser.was_encrypted() && !mime_parser.decoded_data.is_empty() {
            mime_parser.decoded_data.clone()
        } else {
            imf_raw.to_vec()
        }
    } else {
        Vec::new()
    };

    let mut created_db_entries = Vec::with_capacity(mime_parser.parts.len());

    let conn = context.sql.get_conn().await?;

    for part in &mime_parser.parts {
        let mut txt_raw = "".to_string();
        let mut stmt = conn.prepare_cached(
            r#"
INSERT INTO msgs
  (
    rfc724_mid, chat_id,
    from_id, to_id, timestamp, timestamp_sent, 
    timestamp_rcvd, type, state, msgrmsg, 
    txt, subject, txt_raw, param, 
    bytes, mime_headers, mime_in_reply_to,
    mime_references, mime_modified, error, ephemeral_timer,
    ephemeral_timestamp, download_state, hop_info
  )
  VALUES (
    ?, ?, ?, ?,
    ?, ?, ?, ?,
    ?, ?, ?, ?,
    ?, ?, ?, ?,
    ?, ?, ?, ?,
    ?, ?, ?, ?
  );
"#,
        )?;

        let (msg, typ): (&str, Viewtype) = if let Some(better_msg) = &better_msg {
            (better_msg, Viewtype::Text)
        } else {
            (&part.msg, part.typ)
        };

        let part_is_empty = part.msg.is_empty() && part.param.get(Param::Quote).is_none();
        let mime_modified = save_mime_modified && !part_is_empty;
        if mime_modified {
            // Avoid setting mime_modified for more than one part.
            save_mime_modified = false;
        }

        if part.typ == Viewtype::Text {
            let msg_raw = part.msg_raw.as_ref().cloned().unwrap_or_default();
            txt_raw = format!("{}\n\n{}", subject, msg_raw);
        }

        let mut param = part.param.clone();
        if is_system_message != SystemMessage::Unknown {
            param.set_int(Param::Cmd, is_system_message as i32);
        }

        let ephemeral_timestamp = if in_fresh {
            0
        } else {
            match ephemeral_timer {
                EphemeralTimer::Disabled => 0,
                EphemeralTimer::Enabled { duration } => {
                    rcvd_timestamp.saturating_add(duration.into())
                }
            }
        };

        // If you change which information is skipped if the message is trashed,
        // also change `MsgId::trash()` and `delete_expired_messages()`
        let trash = chat_id.is_trash() || (location_kml_is && msg.is_empty());

        stmt.execute(paramsv![
            rfc724_mid,
            if trash { DC_CHAT_ID_TRASH } else { chat_id },
            if trash { ContactId::UNDEFINED } else { from_id },
            if trash { ContactId::UNDEFINED } else { to_id },
            sort_timestamp,
            sent_timestamp,
            rcvd_timestamp,
            typ,
            state,
            is_dc_message,
            if trash { "" } else { msg },
            if trash { "" } else { &subject },
            // txt_raw might contain invalid utf8
            if trash { "" } else { &txt_raw },
            if trash {
                "".to_string()
            } else {
                param.to_string()
            },
            part.bytes as isize,
            if (save_mime_headers || mime_modified) && !trash {
                mime_headers.clone()
            } else {
                Vec::new()
            },
            mime_in_reply_to,
            mime_references,
            mime_modified,
            part.error.as_deref().unwrap_or_default(),
            ephemeral_timer,
            ephemeral_timestamp,
            if is_partial_download.is_some() {
                DownloadState::Available
            } else {
                DownloadState::Done
            },
            mime_parser.hop_info
        ])?;
        let row_id = conn.last_insert_rowid();

        drop(stmt);
        created_db_entries.push(MsgId::new(u32::try_from(row_id)?));
    }
    drop(conn);

    if let Some(replace_msg_id) = replace_msg_id {
        if let Some(created_msg_id) = created_db_entries.pop() {
            context
                .merge_messages(created_msg_id, replace_msg_id)
                .await?;
            created_db_entries.push(replace_msg_id);
        } else {
            replace_msg_id.delete_from_db(context).await?;
        }
    }

    chat_id.unarchive_if_not_muted(context).await?;

    info!(
        context,
        "Message has {} parts and is assigned to chat #{}.", icnt, chat_id,
    );

    // new outgoing message from another device marks the chat as noticed.
    if !incoming && !chat_id.is_special() {
        chat::marknoticed_chat_if_older_than(context, chat_id, sort_timestamp).await?;
    }

    if !is_mdn {
        let mut chat = Chat::load_from_db(context, chat_id).await?;

        // In contrast to most other update-timestamps,
        // use `sort_timestamp` instead of `sent_timestamp` for the subject-timestamp comparison.
        // This way, `LastSubject` actually refers to the most recent message _shown_ in the chat.
        if chat
            .param
            .update_timestamp(Param::SubjectTimestamp, sort_timestamp)?
        {
            // write the last subject even if empty -
            // otherwise a reply may get an outdated subject.
            let subject = mime_parser.get_subject().unwrap_or_default();

            chat.param.set(Param::LastSubject, subject);
            chat.update_param(context).await?;
        }
    }

    if !incoming && is_mdn && is_dc_message == MessengerMessage::Yes {
        // Normally outgoing MDNs sent by us never appear in mailboxes, but Gmail saves all
        // outgoing messages, including MDNs, to the Sent folder. If we detect such saved MDN,
        // delete it.
        needs_delete_job = true;
    }

    Ok(ReceivedMsg {
        chat_id,
        state,
        sort_timestamp,
        msg_ids: created_db_entries,
        needs_delete_job,
    })
}

/// Saves attached locations to the database.
///
/// Emits an event if at least one new location was added.
async fn save_locations(
    context: &Context,
    mime_parser: &MimeMessage,
    chat_id: ChatId,
    from_id: ContactId,
    msg_id: MsgId,
) -> Result<()> {
    if chat_id.is_special() {
        // Do not save locations for trashed messages.
        return Ok(());
    }

    let mut send_event = false;

    if let Some(message_kml) = &mime_parser.message_kml {
        if let Some(newest_location_id) =
            location::save(context, chat_id, from_id, &message_kml.locations, true).await?
        {
            location::set_msg_location_id(context, msg_id, newest_location_id).await?;
            send_event = true;
        }
    }

    if let Some(location_kml) = &mime_parser.location_kml {
        if let Some(addr) = &location_kml.addr {
            let contact = Contact::get_by_id(context, from_id).await?;
            if contact.get_addr().to_lowercase() == addr.to_lowercase() {
                if let Some(newest_location_id) =
                    location::save(context, chat_id, from_id, &location_kml.locations, false)
                        .await?
                {
                    location::set_msg_location_id(context, msg_id, newest_location_id).await?;
                    send_event = true;
                }
            } else {
                warn!(
                    context,
                    "Address in location.kml {:?} is not the same as the sender address {:?}.",
                    addr,
                    contact.get_addr()
                );
            }
        }
    }
    if send_event {
        context.emit_event(EventType::LocationChanged(Some(from_id)));
    }
    Ok(())
}

async fn calc_sort_timestamp(
    context: &Context,
    message_timestamp: i64,
    chat_id: ChatId,
    is_fresh_msg: bool,
) -> Result<i64> {
    let mut sort_timestamp = message_timestamp;

    // get newest non fresh message for this chat
    // update sort_timestamp if less than that
    if is_fresh_msg {
        let last_msg_time: Option<i64> = context
            .sql
            .query_get_value(
                "SELECT MAX(timestamp) FROM msgs WHERE chat_id=? AND state>?",
                paramsv![chat_id, MessageState::InFresh],
            )
            .await?;

        if let Some(last_msg_time) = last_msg_time {
            if last_msg_time > sort_timestamp {
                sort_timestamp = last_msg_time;
            }
        }
    }

    Ok(min(sort_timestamp, smeared_time(context).await))
}

async fn lookup_chat_by_reply(
    context: &Context,
    mime_parser: &MimeMessage,
    parent: &Option<Message>,
    to_ids: &[ContactId],
    from_id: ContactId,
) -> Result<Option<(ChatId, Blocked)>> {
    // Try to assign message to the same chat as the parent message.

    // If this was a private message just to self, it was probably a private reply.
    // It should not go into the group then, but into the private chat.

    if let Some(parent) = parent {
        let parent_chat = Chat::load_from_db(context, parent.chat_id).await?;

        if parent.error.is_some() {
            // If the parent msg is undecipherable, then it may have been assigned to the wrong chat
            // (undecipherable group msgs often get assigned to the 1:1 chat with the sender).
            // We don't have any way of finding out whether a msg is undecipherable, so we check for
            // error.is_some() instead.
            return Ok(None);
        }

        if parent_chat.id == DC_CHAT_ID_TRASH {
            return Ok(None);
        }

        if is_probably_private_reply(context, to_ids, from_id, mime_parser, parent_chat.id).await? {
            return Ok(None);
        }

        info!(
            context,
            "Assigning message to {} as it's a reply to {}", parent_chat.id, parent.rfc724_mid
        );
        return Ok(Some((parent_chat.id, parent_chat.blocked)));
    }

    Ok(None)
}

/// If this method returns true, the message shall be assigned to the 1:1 chat with the sender.
/// If it returns false, it shall be assigned to the parent chat.
async fn is_probably_private_reply(
    context: &Context,
    to_ids: &[ContactId],
    from_id: ContactId,
    mime_parser: &MimeMessage,
    parent_chat_id: ChatId,
) -> Result<bool> {
    // Usually we don't want to show private replies in the parent chat, but in the
    // 1:1 chat with the sender.
    //
    // There is one exception: Classical MUA replies to two-member groups
    // should be assigned to the group chat. We restrict this exception to classical emails, as chat-group-messages
    // contain a Chat-Group-Id header and can be sorted into the correct chat this way.

    let private_message =
        (to_ids == [ContactId::SELF]) || (from_id == ContactId::SELF && to_ids.len() == 1);
    if !private_message {
        return Ok(false);
    }

    if !mime_parser.has_chat_version() {
        let chat_contacts = chat::get_chat_contacts(context, parent_chat_id).await?;
        if chat_contacts.len() == 2 && chat_contacts.contains(&ContactId::SELF) {
            return Ok(false);
        }
    }

    Ok(true)
}

/// This function tries to extract the group-id from the message and returns the corresponding
/// chat_id. If the chat does not exist, it is created. If there is no group-id and there are more
/// than two members, a new ad hoc group is created.
///
/// On success the function returns the found/created (chat_id, chat_blocked) tuple.
async fn create_or_lookup_group(
    context: &Context,
    mime_parser: &mut MimeMessage,
    allow_creation: bool,
    create_blocked: Blocked,
    from_id: ContactId,
    to_ids: &[ContactId],
) -> Result<Option<(ChatId, Blocked)>> {
    let grpid = if let Some(grpid) = try_getting_grpid(mime_parser) {
        grpid
    } else if allow_creation {
        let mut member_ids: Vec<ContactId> = to_ids.to_vec();
        if !member_ids.contains(&(from_id)) {
            member_ids.push(from_id);
        }
        if !member_ids.contains(&(ContactId::SELF)) {
            member_ids.push(ContactId::SELF);
        }

        let res = create_adhoc_group(context, mime_parser, create_blocked, &member_ids)
            .await
            .context("could not create ad hoc group")?
            .map(|chat_id| (chat_id, create_blocked));
        return Ok(res);
    } else {
        info!(context, "creating ad-hoc group prevented from caller");
        return Ok(None);
    };

    let mut chat_id;
    let mut chat_id_blocked;
    if let Some((id, _protected, blocked)) = chat::get_chat_id_by_grpid(context, &grpid).await? {
        chat_id = Some(id);
        chat_id_blocked = blocked;
    } else {
        chat_id = None;
        chat_id_blocked = Default::default();
    }

    // For chat messages, we don't have to guess (is_*probably*_private_reply()) but we know for sure that
    // they belong to the group because of the Chat-Group-Id or Message-Id header
    if let Some(chat_id) = chat_id {
        if !mime_parser.has_chat_version()
            && is_probably_private_reply(context, to_ids, from_id, mime_parser, chat_id).await?
        {
            return Ok(None);
        }
    }

    let create_protected = if mime_parser.get_header(HeaderDef::ChatVerified).is_some() {
        if let Err(err) = check_verified_properties(context, mime_parser, from_id, to_ids).await {
            warn!(context, "verification problem: {}", err);
            let s = format!("{}. See 'Info' for more details", err);
            mime_parser.repl_msg_by_error(&s);
        }
        ProtectionStatus::Protected
    } else {
        ProtectionStatus::Unprotected
    };

    async fn self_explicitly_added(
        context: &Context,
        mime_parser: &&mut MimeMessage,
    ) -> Result<bool> {
        let ret = match mime_parser.get_header(HeaderDef::ChatGroupMemberAdded) {
            Some(member_addr) => context.is_self_addr(member_addr).await?,
            None => false,
        };
        Ok(ret)
    }

    if chat_id.is_none()
            && !mime_parser.is_mailinglist_message()
            && !grpid.is_empty()
            && mime_parser.get_header(HeaderDef::ChatGroupName).is_some()
            // otherwise, a pending "quit" message may pop up
            && mime_parser.get_header(HeaderDef::ChatGroupMemberRemoved).is_none()
            // re-create explicitly left groups only if ourself is re-added
            && (!chat::is_group_explicitly_left(context, &grpid).await?
                || self_explicitly_added(context, &mime_parser).await?)
    {
        // Group does not exist but should be created.
        if !allow_creation {
            info!(context, "creating group forbidden by caller");
            return Ok(None);
        }

        let grpname = mime_parser
            .get_header(HeaderDef::ChatGroupName)
            .context("Chat-Group-Name vanished")?;
        let new_chat_id = ChatId::create_multiuser_record(
            context,
            Chattype::Group,
            &grpid,
            grpname,
            create_blocked,
            create_protected,
            None,
        )
        .await
        .with_context(|| format!("Failed to create group '{}' for grpid={}", grpname, grpid))?;

        chat_id = Some(new_chat_id);
        chat_id_blocked = create_blocked;

        // Create initial member list.
        chat::add_to_chat_contacts_table(context, new_chat_id, ContactId::SELF).await?;
        if !from_id.is_special() && !chat::is_contact_in_chat(context, new_chat_id, from_id).await?
        {
            chat::add_to_chat_contacts_table(context, new_chat_id, from_id).await?;
        }
        for &to_id in to_ids.iter() {
            info!(context, "adding to={:?} to chat id={}", to_id, new_chat_id);
            if to_id != ContactId::SELF
                && !chat::is_contact_in_chat(context, new_chat_id, to_id).await?
            {
                chat::add_to_chat_contacts_table(context, new_chat_id, to_id).await?;
            }
        }

        // once, we have protected-chats explained in UI, we can uncomment the following lines.
        // ("verified groups" did not add a message anyway)
        //
        //if create_protected == ProtectionStatus::Protected {
        // set from_id=0 as it is not clear that the sender of this random group message
        // actually really has enabled chat-protection at some point.
        //new_chat_id
        //    .add_protection_msg(context, ProtectionStatus::Protected, false, 0)
        //    .await?;
        //}

        context.emit_event(EventType::ChatModified(new_chat_id));
    }

    if let Some(chat_id) = chat_id {
        Ok(Some((chat_id, chat_id_blocked)))
    } else if mime_parser.decrypting_failed {
        // It is possible that the message was sent to a valid,
        // yet unknown group, which was rejected because
        // Chat-Group-Name, which is in the encrypted part, was
        // not found. We can't create a properly named group in
        // this case, so assign error message to 1:1 chat with the
        // sender instead.
        Ok(None)
    } else {
        // The message was decrypted successfully, but contains a late "quit" or otherwise
        // unwanted message.
        info!(context, "message belongs to unwanted group (TRASH)");
        Ok(Some((DC_CHAT_ID_TRASH, Blocked::Not)))
    }
}

/// Apply group member list, name, avatar and protection status changes from the MIME message.
///
/// Optionally returns better message to replace the original system message.
async fn apply_group_changes(
    context: &Context,
    mime_parser: &mut MimeMessage,
    sent_timestamp: i64,
    chat_id: ChatId,
    from_id: ContactId,
    to_ids: &[ContactId],
) -> Result<Option<String>> {
    let mut chat = Chat::load_from_db(context, chat_id).await?;
    if chat.typ != Chattype::Group {
        return Ok(None);
    }

    let mut recreate_member_list = false;
    let mut send_event_chat_modified = false;

    let mut better_msg = None;
    let removed_id;
    if let Some(removed_addr) = mime_parser
        .get_header(HeaderDef::ChatGroupMemberRemoved)
        .cloned()
    {
        removed_id = Contact::lookup_id_by_addr(context, &removed_addr, Origin::Unknown).await?;
        recreate_member_list = true;
        match removed_id {
            Some(contact_id) => {
                better_msg = if contact_id == from_id {
                    Some(stock_str::msg_group_left(context, from_id).await)
                } else {
                    Some(stock_str::msg_del_member(context, &removed_addr, from_id).await)
                };
            }
            None => warn!(context, "removed {:?} has no contact_id", removed_addr),
        }
    } else {
        removed_id = None;
        if let Some(added_member) = mime_parser
            .get_header(HeaderDef::ChatGroupMemberAdded)
            .cloned()
        {
            better_msg = Some(stock_str::msg_add_member(context, &added_member, from_id).await);
            recreate_member_list = true;
        } else if let Some(old_name) = mime_parser.get_header(HeaderDef::ChatGroupNameChanged) {
            if let Some(grpname) = mime_parser
                .get_header(HeaderDef::ChatGroupName)
                .filter(|grpname| grpname.len() < 200)
            {
                if chat_id
                    .update_timestamp(context, Param::GroupNameTimestamp, sent_timestamp)
                    .await?
                {
                    info!(context, "updating grpname for chat {}", chat_id);
                    context
                        .sql
                        .execute(
                            "UPDATE chats SET name=? WHERE id=?;",
                            paramsv![grpname.to_string(), chat_id],
                        )
                        .await?;
                    send_event_chat_modified = true;
                }

                better_msg =
                    Some(stock_str::msg_grp_name(context, old_name, grpname, from_id).await);
            }
        } else if let Some(value) = mime_parser.get_header(HeaderDef::ChatContent) {
            if value == "group-avatar-changed" {
                if let Some(avatar_action) = &mime_parser.group_avatar {
                    // this is just an explicit message containing the group-avatar,
                    // apart from that, the group-avatar is send along with various other messages
                    better_msg = match avatar_action {
                        AvatarAction::Delete => {
                            Some(stock_str::msg_grp_img_deleted(context, from_id).await)
                        }
                        AvatarAction::Change(_) => {
                            Some(stock_str::msg_grp_img_changed(context, from_id).await)
                        }
                    };
                }
            }
        }
    }

    if mime_parser.get_header(HeaderDef::ChatVerified).is_some() {
        if let Err(err) = check_verified_properties(context, mime_parser, from_id, to_ids).await {
            warn!(context, "verification problem: {}", err);
            let s = format!("{}. See 'Info' for more details", err);
            mime_parser.repl_msg_by_error(&s);
        }

        if !chat.is_protected() {
            chat_id
                .inner_set_protection(context, ProtectionStatus::Protected)
                .await?;
            recreate_member_list = true;
        }
    }

    // add members to group/check members
    if recreate_member_list {
        if chat::is_contact_in_chat(context, chat_id, ContactId::SELF).await?
            && !chat::is_contact_in_chat(context, chat_id, from_id).await?
        {
            warn!(
                context,
                "Contact {} attempts to modify group chat {} member list without being a member.",
                from_id,
                chat_id
            );
        } else if chat_id
            .update_timestamp(context, Param::MemberListTimestamp, sent_timestamp)
            .await?
        {
            if removed_id.is_some()
                || !chat::is_contact_in_chat(context, chat_id, ContactId::SELF).await?
            {
                // Members could have been removed while we were
                // absent. We can't use existing member list and need to
                // start from scratch.
                context
                    .sql
                    .execute(
                        "DELETE FROM chats_contacts WHERE chat_id=?;",
                        paramsv![chat_id],
                    )
                    .await?;

                if removed_id != Some(ContactId::SELF) {
                    chat::add_to_chat_contacts_table(context, chat_id, ContactId::SELF).await?;
                }
            }
            if !from_id.is_special()
                && from_id != ContactId::SELF
                && !chat::is_contact_in_chat(context, chat_id, from_id).await?
                && removed_id != Some(from_id)
            {
                chat::add_to_chat_contacts_table(context, chat_id, from_id).await?;
            }
            for &to_id in to_ids.iter() {
                if to_id != ContactId::SELF
                    && !chat::is_contact_in_chat(context, chat_id, to_id).await?
                    && removed_id != Some(to_id)
                {
                    info!(context, "adding to={:?} to chat id={}", to_id, chat_id);
                    chat::add_to_chat_contacts_table(context, chat_id, to_id).await?;
                }
            }
            send_event_chat_modified = true;
        }
    }

    if let Some(avatar_action) = &mime_parser.group_avatar {
        if !chat::is_contact_in_chat(context, chat_id, ContactId::SELF).await? {
            warn!(
                context,
                "Received group avatar update for group chat {} we are not a member of.", chat_id
            );
        } else if !chat::is_contact_in_chat(context, chat_id, from_id).await? {
            warn!(
                context,
                "Contact {} attempts to modify group chat {} avatar without being a member.",
                from_id,
                chat_id
            );
        } else {
            info!(context, "group-avatar change for {}", chat_id);
            if chat
                .param
                .update_timestamp(Param::AvatarTimestamp, sent_timestamp)?
            {
                match avatar_action {
                    AvatarAction::Change(profile_image) => {
                        chat.param.set(Param::ProfileImage, profile_image);
                    }
                    AvatarAction::Delete => {
                        chat.param.remove(Param::ProfileImage);
                    }
                };
                chat.update_param(context).await?;
                send_event_chat_modified = true;
            }
        }
    }

    if send_event_chat_modified {
        context.emit_event(EventType::ChatModified(chat_id));
    }
    Ok(better_msg)
}

/// Create or lookup a mailing list chat.
///
/// `list_id_header` contains the Id that must be used for the mailing list
/// and has the form `Name <Id>`, `<Id>` or just `Id`.
/// Depending on the mailing list type, `list_id_header`
/// was picked from `ListId:`-header or the `Sender:`-header.
///
/// `mime_parser` is the corresponding message
/// and is used to figure out the mailing list name from different header fields.
#[allow(clippy::indexing_slicing)]
async fn create_or_lookup_mailinglist(
    context: &Context,
    allow_creation: bool,
    list_id_header: &str,
    mime_parser: &MimeMessage,
) -> Result<Option<(ChatId, Blocked)>> {
    static LIST_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(.+)<(.+)>$").unwrap());
    let (mut name, listid) = match LIST_ID.captures(list_id_header) {
        Some(cap) => (cap[1].trim().to_string(), cap[2].trim().to_string()),
        None => (
            "".to_string(),
            list_id_header
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string(),
        ),
    };

    if let Some((chat_id, _, blocked)) = chat::get_chat_id_by_grpid(context, &listid).await? {
        return Ok(Some((chat_id, blocked)));
    }

    // for mailchimp lists, the name in `ListId` is just a long number.
    // a usable name for these lists is in the `From` header
    // and we can detect these lists by a unique `ListId`-suffix.
    if listid.ends_with(".list-id.mcsv.net") {
        if let Some(from) = mime_parser.from.first() {
            if let Some(display_name) = &from.display_name {
                name = display_name.clone();
            }
        }
    }

    // additional names in square brackets in the subject are preferred
    // (as that part is much more visible, we assume, that names is shorter and comes more to the point,
    // than the sometimes longer part from ListId)
    let subject = mime_parser.get_subject().unwrap_or_default();
    static SUBJECT: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^.{0,5}\[(.+?)\](\s*\[.+\])?").unwrap()); // remove square brackets around first name
    if let Some(cap) = SUBJECT.captures(&subject) {
        name = cap[1].to_string() + cap.get(2).map_or("", |m| m.as_str());
    }

    // if we do not have a name yet and `From` indicates, that this is a notification list,
    // a usable name is often in the `From` header (seen for several parcel service notifications).
    // same, if we do not have a name yet and `List-Id` has a known suffix (`.xt.local`)
    //
    // this pattern is similar to mailchimp above, however,
    // with weaker conditions and does not overwrite existing names.
    if name.is_empty() {
        if let Some(from) = mime_parser.from.first() {
            if from.addr.contains("noreply")
                || from.addr.contains("no-reply")
                || from.addr.starts_with("notifications@")
                || from.addr.starts_with("newsletter@")
                || listid.ends_with(".xt.local")
            {
                if let Some(display_name) = &from.display_name {
                    name = display_name.clone();
                }
            }
        }
    }

    // as a last resort, use the ListId as the name
    // but strip some known, long hash prefixes
    if name.is_empty() {
        // 51231231231231231231231232869f58.xing.com -> xing.com
        static PREFIX_32_CHARS_HEX: Lazy<Regex> =
            Lazy::new(|| Regex::new(r"([0-9a-fA-F]{32})\.(.{6,})").unwrap());
        if let Some(cap) = PREFIX_32_CHARS_HEX.captures(&listid) {
            name = cap[2].to_string();
        } else {
            name = listid.clone();
        }
    }

    if allow_creation {
        // list does not exist but should be created
        let param = mime_parser.list_post.as_ref().map(|list_post| {
            let mut p = Params::new();
            p.set(Param::ListPost, list_post);
            p.to_string()
        });

        let chat_id = ChatId::create_multiuser_record(
            context,
            Chattype::Mailinglist,
            &listid,
            &name,
            Blocked::Request,
            ProtectionStatus::Unprotected,
            param,
        )
        .await
        .with_context(|| {
            format!(
                "Failed to create mailinglist '{}' for grpid={}",
                &name, &listid
            )
        })?;

        chat::add_to_chat_contacts_table(context, chat_id, ContactId::SELF).await?;
        Ok(Some((chat_id, Blocked::Request)))
    } else {
        info!(context, "creating list forbidden by caller");
        Ok(None)
    }
}

/// Set ListId param on the contact and ListPost param the chat.
/// Only called for incoming messages since outgoing messages never have a
/// List-Post header, anyway.
async fn apply_mailinglist_changes(
    context: &Context,
    mime_parser: &MimeMessage,
    chat_id: ChatId,
) -> Result<()> {
    if let Some(list_post) = &mime_parser.list_post {
        let mut chat = Chat::load_from_db(context, chat_id).await?;
        if chat.typ != Chattype::Mailinglist {
            return Ok(());
        }
        let listid = &chat.grpid;

        let (contact_id, _) =
            Contact::add_or_lookup(context, "", list_post, Origin::Hidden).await?;
        let mut contact = Contact::load_from_db(context, contact_id).await?;
        if contact.param.get(Param::ListId) != Some(listid.as_str()) {
            contact.param.set(Param::ListId, &listid);
            contact.update_param(context).await?;
        }

        if let Some(old_list_post) = chat.param.get(Param::ListPost) {
            if list_post != old_list_post {
                // Apparently the mailing list is using a different List-Post header in each message.
                // Make the mailing list read-only because we would't know which message the user wants to reply to.
                chat.param.set(Param::ListPost, "");
                chat.update_param(context).await?;
            }
        } else {
            chat.param.set(Param::ListPost, list_post);
            chat.update_param(context).await?;
        }
    }

    Ok(())
}

fn try_getting_grpid(mime_parser: &MimeMessage) -> Option<String> {
    if let Some(optional_field) = mime_parser.get_header(HeaderDef::ChatGroupId) {
        return Some(optional_field.clone());
    }

    // Useful for undecipherable messages sent to known group.
    if let Some(extracted_grpid) = extract_grpid(mime_parser, HeaderDef::MessageId) {
        return Some(extracted_grpid.to_string());
    }

    if !mime_parser.has_chat_version() {
        if let Some(extracted_grpid) = extract_grpid(mime_parser, HeaderDef::InReplyTo) {
            return Some(extracted_grpid.to_string());
        } else if let Some(extracted_grpid) = extract_grpid(mime_parser, HeaderDef::References) {
            return Some(extracted_grpid.to_string());
        }
    }

    None
}

/// try extract a grpid from a message-id list header value
fn extract_grpid(mime_parser: &MimeMessage, headerdef: HeaderDef) -> Option<&str> {
    let header = mime_parser.get_header(headerdef)?;
    let parts = header
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    parts.filter_map(extract_grpid_from_rfc724_mid).next()
}

/// Creates ad-hoc group and returns chat ID on success.
async fn create_adhoc_group(
    context: &Context,
    mime_parser: &MimeMessage,
    create_blocked: Blocked,
    member_ids: &[ContactId],
) -> Result<Option<ChatId>> {
    if mime_parser.is_mailinglist_message() {
        info!(
            context,
            "not creating ad-hoc group for mailing list message"
        );

        return Ok(None);
    }

    if mime_parser.decrypting_failed {
        // Do not create a new ad-hoc group if the message cannot be
        // decrypted.
        //
        // The subject may be encrypted and contain a placeholder such
        // as "...". It can also be a COI group, with encrypted
        // Chat-Group-ID and incompatible Message-ID format.
        //
        // Instead, assign the message to 1:1 chat with the sender.
        warn!(
            context,
            "not creating ad-hoc group for message that cannot be decrypted"
        );
        return Ok(None);
    }

    if member_ids.len() < 3 {
        info!(context, "not creating ad-hoc group: too few contacts");
        return Ok(None);
    }

    // use subject as initial chat name
    let grpname = mime_parser
        .get_subject()
        .unwrap_or_else(|| "Unnamed group".to_string());

    let new_chat_id: ChatId = ChatId::create_multiuser_record(
        context,
        Chattype::Group,
        "", // Ad hoc groups have no ID.
        &grpname,
        create_blocked,
        ProtectionStatus::Unprotected,
        None,
    )
    .await?;
    for &member_id in member_ids.iter() {
        chat::add_to_chat_contacts_table(context, new_chat_id, member_id).await?;
    }

    context.emit_event(EventType::ChatModified(new_chat_id));

    Ok(Some(new_chat_id))
}

async fn check_verified_properties(
    context: &Context,
    mimeparser: &MimeMessage,
    from_id: ContactId,
    to_ids: &[ContactId],
) -> Result<()> {
    let contact = Contact::load_from_db(context, from_id).await?;

    ensure!(mimeparser.was_encrypted(), "This message is not encrypted.");

    if mimeparser.get_header(HeaderDef::ChatVerified).is_none() {
        // we do not fail here currently, this would exclude (a) non-deltas
        // and (b) deltas with different protection views across multiple devices.
        // for group creation or protection enabled/disabled, however, Chat-Verified is respected.
        warn!(
            context,
            "{} did not mark message as protected.",
            contact.get_addr()
        );
    }

    // ensure, the contact is verified
    // and the message is signed with a verified key of the sender.
    // this check is skipped for SELF as there is no proper SELF-peerstate
    // and results in group-splits otherwise.
    if from_id != ContactId::SELF {
        let peerstate = Peerstate::from_addr(context, contact.get_addr()).await?;

        if peerstate.is_none()
            || contact.is_verified_ex(context, peerstate.as_ref()).await?
                != VerifiedStatus::BidirectVerified
        {
            bail!(
                "Sender of this message is not verified: {}",
                contact.get_addr()
            );
        }

        if let Some(peerstate) = peerstate {
            ensure!(
                peerstate.has_verified_key(&mimeparser.signatures),
                "The message was sent with non-verified encryption."
            );
        }
    }

    // we do not need to check if we are verified with ourself
    let to_ids = to_ids
        .iter()
        .copied()
        .filter(|id| *id != ContactId::SELF)
        .collect::<Vec<ContactId>>();

    if to_ids.is_empty() {
        return Ok(());
    }

    let rows = context
        .sql
        .query_map(
            &format!(
                "SELECT c.addr, LENGTH(ps.verified_key_fingerprint)  FROM contacts c  \
             LEFT JOIN acpeerstates ps ON c.addr=ps.addr  WHERE c.id IN({}) ",
                sql::repeat_vars(to_ids.len())
            ),
            rusqlite::params_from_iter(to_ids),
            |row| {
                let to_addr: String = row.get(0)?;
                let is_verified: i32 = row.get(1).unwrap_or(0);
                Ok((to_addr, is_verified != 0))
            },
            |rows| {
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Into::into)
            },
        )
        .await?;

    for (to_addr, mut is_verified) in rows.into_iter() {
        info!(
            context,
            "check_verified_properties: {:?} self={:?}",
            to_addr,
            context.is_self_addr(&to_addr).await
        );
        let peerstate = Peerstate::from_addr(context, &to_addr).await?;

        // mark gossiped keys (if any) as verified
        if mimeparser.gossiped_addr.contains(&to_addr) {
            if let Some(mut peerstate) = peerstate {
                // if we're here, we know the gossip key is verified:
                // - use the gossip-key as verified-key if there is no verified-key
                // - OR if the verified-key does not match public-key or gossip-key
                //   (otherwise a verified key can _only_ be updated through QR scan which might be annoying,
                //   see <https://github.com/nextleap-project/countermitm/issues/46> for a discussion about this point)
                if !is_verified
                    || peerstate.verified_key_fingerprint != peerstate.public_key_fingerprint
                        && peerstate.verified_key_fingerprint != peerstate.gossip_key_fingerprint
                {
                    info!(context, "{} has verified {}.", contact.get_addr(), to_addr,);
                    let fp = peerstate.gossip_key_fingerprint.clone();
                    if let Some(fp) = fp {
                        peerstate.set_verified(
                            PeerstateKeyType::GossipKey,
                            &fp,
                            PeerstateVerifiedStatus::BidirectVerified,
                        );
                        peerstate.save_to_db(&context.sql, false).await?;
                        is_verified = true;
                    }
                }
            }
        }
        if !is_verified {
            bail!(
                "{} is not a member of this protected chat",
                to_addr.to_string()
            );
        }
    }
    Ok(())
}

/// Returns the last message referenced from `References` header if it is in the database.
///
/// For Delta Chat messages it is the last message in the chat of the sender.
///
/// Note that the returned message may be trashed.
async fn get_previous_message(
    context: &Context,
    mime_parser: &MimeMessage,
) -> Result<Option<Message>> {
    if let Some(field) = mime_parser.get_header(HeaderDef::References) {
        if let Some(rfc724mid) = parse_message_ids(field).last() {
            if let Some(msg_id) = rfc724_mid_exists(context, rfc724mid).await? {
                return Ok(Some(Message::load_from_db(context, msg_id).await?));
            }
        }
    }
    Ok(None)
}

/// Given a list of Message-IDs, returns the latest message found in the database.
///
/// Only messages that are not in the trash chat are considered.
async fn get_rfc724_mid_in_list(context: &Context, mid_list: &str) -> Result<Option<Message>> {
    if mid_list.is_empty() {
        return Ok(None);
    }

    for id in parse_message_ids(mid_list).iter().rev() {
        if let Some(msg_id) = rfc724_mid_exists(context, id).await? {
            let msg = Message::load_from_db(context, msg_id).await?;
            if msg.chat_id != DC_CHAT_ID_TRASH {
                return Ok(Some(msg));
            }
        }
    }

    Ok(None)
}

/// Returns the last message referenced from References: header found in the database.
///
/// If none found, tries In-Reply-To: as a fallback for classic MUAs that don't set the
/// References: header.
// TODO also save first entry of References and look for this?
async fn get_parent_message(
    context: &Context,
    mime_parser: &MimeMessage,
) -> Result<Option<Message>> {
    if let Some(field) = mime_parser.get_header(HeaderDef::References) {
        if let Some(msg) = get_rfc724_mid_in_list(context, field).await? {
            return Ok(Some(msg));
        }
    }

    if let Some(field) = mime_parser.get_header(HeaderDef::InReplyTo) {
        if let Some(msg) = get_rfc724_mid_in_list(context, field).await? {
            return Ok(Some(msg));
        }
    }

    Ok(None)
}

pub(crate) async fn get_prefetch_parent_message(
    context: &Context,
    headers: &[mailparse::MailHeader<'_>],
) -> Result<Option<Message>> {
    if let Some(field) = headers.get_header_value(HeaderDef::References) {
        if let Some(msg) = get_rfc724_mid_in_list(context, &field).await? {
            return Ok(Some(msg));
        }
    }

    if let Some(field) = headers.get_header_value(HeaderDef::InReplyTo) {
        if let Some(msg) = get_rfc724_mid_in_list(context, &field).await? {
            return Ok(Some(msg));
        }
    }

    Ok(None)
}

/// Looks up contact IDs from the database given the list of recipients.
///
/// Returns vector of IDs guaranteed to be unique.
///
/// * param `prevent_rename`: if true, the display_name of this contact will not be changed. Useful for
/// mailing lists: In some mailing lists, many users write from the same address but with different
/// display names. We don't want the display name to change everytime the user gets a new email from
/// a mailing list.
async fn add_or_lookup_contacts_by_address_list(
    context: &Context,
    address_list: &[SingleInfo],
    origin: Origin,
    prevent_rename: bool,
) -> Result<Vec<ContactId>> {
    let mut contact_ids = HashSet::new();
    for info in address_list.iter() {
        let addr = &info.addr;
        if !may_be_valid_addr(addr) {
            continue;
        }
        let display_name = if prevent_rename {
            Some("")
        } else {
            info.display_name.as_deref()
        };
        contact_ids
            .insert(add_or_lookup_contact_by_addr(context, display_name, addr, origin).await?);
    }

    Ok(contact_ids.into_iter().collect::<Vec<ContactId>>())
}

/// Add contacts to database on receiving messages.
async fn add_or_lookup_contact_by_addr(
    context: &Context,
    display_name: Option<&str>,
    addr: &str,
    origin: Origin,
) -> Result<ContactId> {
    if context.is_self_addr(addr).await? {
        return Ok(ContactId::SELF);
    }
    let display_name_normalized = display_name.map(normalize_name).unwrap_or_default();

    let (row_id, _modified) =
        Contact::add_or_lookup(context, &display_name_normalized, addr, origin).await?;
    Ok(row_id)
}

#[cfg(test)]
mod tests {
    use tokio::fs;

    use super::*;

    use crate::chat::get_chat_contacts;
    use crate::chat::{get_chat_msgs, ChatItem, ChatVisibility};
    use crate::chatlist::Chatlist;
    use crate::constants::DC_GCL_NO_SPECIALS;
    use crate::imap::prefetch_should_download;
    use crate::message::Message;
    use crate::test_utils::{get_chat_msg, TestContext, TestContextManager};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_grpid_simple() {
        let context = TestContext::new().await;
        let raw = b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                    From: hello\n\
                    Subject: outer-subject\n\
                    In-Reply-To: <lqkjwelq123@123123>\n\
                    References: <Gr.HcxyMARjyJy.9-uvzWPTLtV@nauta.cu>\n\
                    \n\
                    hello\x00";
        let mimeparser = MimeMessage::from_bytes(&context.ctx, &raw[..])
            .await
            .unwrap();
        assert_eq!(extract_grpid(&mimeparser, HeaderDef::InReplyTo), None);
        let grpid = Some("HcxyMARjyJy");
        assert_eq!(extract_grpid(&mimeparser, HeaderDef::References), grpid);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_grpid_from_multiple() {
        let context = TestContext::new().await;
        let raw = b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                    From: hello\n\
                    Subject: outer-subject\n\
                    In-Reply-To: <Gr.HcxyMARjyJy.9-qweqwe@asd.net>\n\
                    References: <qweqweqwe>, <Gr.HcxyMARjyJy.9-uvzWPTLtV@nau.ca>\n\
                    \n\
                    hello\x00";
        let mimeparser = MimeMessage::from_bytes(&context.ctx, &raw[..])
            .await
            .unwrap();
        let grpid = Some("HcxyMARjyJy");
        assert_eq!(extract_grpid(&mimeparser, HeaderDef::InReplyTo), grpid);
        assert_eq!(extract_grpid(&mimeparser, HeaderDef::References), grpid);
    }

    static MSGRMSG: &[u8] =
        b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                    From: Bob <bob@example.com>\n\
                    To: alice@example.org\n\
                    Chat-Version: 1.0\n\
                    Subject: Chat: hello\n\
                    Message-ID: <Mr.1111@example.com>\n\
                    Date: Sun, 22 Mar 2020 22:37:55 +0000\n\
                    \n\
                    hello\n";

    static ONETOONE_NOREPLY_MAIL: &[u8] =
        b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                    From: Bob <bob@example.com>\n\
                    To: alice@example.org\n\
                    Subject: Chat: hello\n\
                    Message-ID: <2222@example.com>\n\
                    Date: Sun, 22 Mar 2020 22:37:56 +0000\n\
                    \n\
                    hello\n";

    static GRP_MAIL: &[u8] =
        b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                    From: bob@example.com\n\
                    To: alice@example.org, claire@example.com\n\
                    Subject: group with Alice, Bob and Claire\n\
                    Message-ID: <3333@example.com>\n\
                    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                    \n\
                    hello\n";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_adhoc_group_show_chats_only() {
        let t = TestContext::new_alice().await;
        assert_eq!(t.get_config_int(Config::ShowEmails).await.unwrap(), 0);

        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0);

        receive_imf(&t, MSGRMSG, false).await.unwrap();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);

        receive_imf(&t, ONETOONE_NOREPLY_MAIL, false).await.unwrap();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);

        receive_imf(&t, GRP_MAIL, false).await.unwrap();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_adhoc_group_show_accepted_contact_unknown() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("1")).await.unwrap();
        receive_imf(&t, GRP_MAIL, false).await.unwrap();

        // adhoc-group with unknown contacts with show_emails=accepted is ignored for unknown contacts
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_adhoc_group_show_accepted_contact_known() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("1")).await.unwrap();
        Contact::create(&t, "Bob", "bob@example.com").await.unwrap();
        receive_imf(&t, GRP_MAIL, false).await.unwrap();

        // adhoc-group with known contacts with show_emails=accepted is still ignored for known contacts
        // (and existent chat is required)
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_adhoc_group_show_accepted_contact_accepted() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("1")).await.unwrap();

        // accept Bob by accepting a delta-message from Bob
        receive_imf(&t, MSGRMSG, false).await.unwrap();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);
        let chat_id = chats.get_chat_id(0).unwrap();
        assert!(!chat_id.is_special());
        let chat = chat::Chat::load_from_db(&t, chat_id).await.unwrap();
        assert!(chat.is_contact_request());
        chat_id.accept(&t).await.unwrap();
        let chat = chat::Chat::load_from_db(&t, chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Single);
        assert_eq!(chat.name, "Bob");
        assert_eq!(chat::get_chat_contacts(&t, chat_id).await.unwrap().len(), 1);
        assert_eq!(chat::get_chat_msgs(&t, chat_id, 0).await.unwrap().len(), 1);

        // receive a non-delta-message from Bob, shows up because of the show_emails setting
        receive_imf(&t, ONETOONE_NOREPLY_MAIL, false).await.unwrap();

        assert_eq!(chat::get_chat_msgs(&t, chat_id, 0).await.unwrap().len(), 2);

        // let Bob create an adhoc-group by a non-delta-message, shows up because of the show_emails setting
        receive_imf(&t, GRP_MAIL, false).await.unwrap();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 2);
        let chat_id = chats.get_chat_id(0).unwrap();
        let chat = chat::Chat::load_from_db(&t, chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Group);
        assert_eq!(chat.name, "group with Alice, Bob and Claire");
        assert_eq!(chat::get_chat_contacts(&t, chat_id).await.unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_adhoc_group_show_all() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();
        receive_imf(&t, GRP_MAIL, false).await.unwrap();

        // adhoc-group with unknown contacts with show_emails=all will show up in a single chat
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);
        let chat_id = chats.get_chat_id(0).unwrap();
        let chat = chat::Chat::load_from_db(&t, chat_id).await.unwrap();
        assert!(chat.is_contact_request());
        chat_id.accept(&t).await.unwrap();
        let chat = chat::Chat::load_from_db(&t, chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Group);
        assert_eq!(chat.name, "group with Alice, Bob and Claire");
        assert_eq!(chat::get_chat_contacts(&t, chat_id).await.unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_read_receipt_and_unarchive() -> Result<()> {
        // create alice's account
        let t = TestContext::new_alice().await;

        let bob_id = Contact::create(&t, "bob", "bob@example.com").await?;
        let one2one_id = ChatId::create_for_contact(&t, bob_id).await?;
        one2one_id
            .set_visibility(&t, ChatVisibility::Archived)
            .await
            .unwrap();
        let one2one = Chat::load_from_db(&t, one2one_id).await?;
        assert!(one2one.get_visibility() == ChatVisibility::Archived);

        // create a group with bob, archive group
        let group_id = chat::create_group_chat(&t, ProtectionStatus::Unprotected, "foo").await?;
        chat::add_contact_to_chat(&t, group_id, bob_id).await?;
        assert_eq!(chat::get_chat_msgs(&t, group_id, 0).await.unwrap().len(), 0);
        group_id
            .set_visibility(&t, ChatVisibility::Archived)
            .await?;
        let group = Chat::load_from_db(&t, group_id).await?;
        assert!(group.get_visibility() == ChatVisibility::Archived);

        // everything archived, chatlist should be empty
        assert_eq!(
            Chatlist::try_load(&t, DC_GCL_NO_SPECIALS, None, None)
                .await?
                .len(),
            0
        );

        // send a message to group with bob
        receive_imf(
            &t,
            format!(
                "Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@example.org\n\
                 To: bob@example.com\n\
                 Subject: foo\n\
                 Message-ID: <Gr.{}.12345678901@example.com>\n\
                 Chat-Version: 1.0\n\
                 Chat-Group-ID: {}\n\
                 Chat-Group-Name: foo\n\
                 Chat-Disposition-Notification-To: alice@example.org\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello\n",
                group.grpid, group.grpid
            )
            .as_bytes(),
            false,
        )
        .await?;
        let msg = get_chat_msg(&t, group_id, 0, 1).await;
        assert_eq!(msg.is_dc_message, MessengerMessage::Yes);
        assert_eq!(msg.text.unwrap(), "hello");
        assert_eq!(msg.state, MessageState::OutDelivered);
        let group = Chat::load_from_db(&t, group_id).await?;
        assert!(group.get_visibility() == ChatVisibility::Normal);

        // bob sends a read receipt to the group
        receive_imf(
            &t,
            format!(
                "Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: bob@example.com\n\
                 To: alice@example.org\n\
                 Subject: message opened\n\
                 Date: Sun, 22 Mar 2020 23:37:57 +0000\n\
                 Chat-Version: 1.0\n\
                 Message-ID: <Mr.12345678902@example.com>\n\
                 Content-Type: multipart/report; report-type=disposition-notification; boundary=\"SNIPP\"\n\
                 \n\
                 \n\
                 --SNIPP\n\
                 Content-Type: text/plain; charset=utf-8\n\
                 \n\
                 Read receipts do not guarantee sth. was read.\n\
                 \n\
                 \n\
                 --SNIPP\n\
                 Content-Type: message/disposition-notification\n\
                 \n\
                 Reporting-UA: Delta Chat 1.28.0\n\
                 Original-Recipient: rfc822;bob@example.com\n\
                 Final-Recipient: rfc822;bob@example.com\n\
                 Original-Message-ID: <Gr.{}.12345678901@example.com>\n\
                 Disposition: manual-action/MDN-sent-automatically; displayed\n\
                 \n\
                 \n\
                 --SNIPP--",
                group.grpid
            )
            .as_bytes(),
            false,
        )
        .await?;
        assert_eq!(chat::get_chat_msgs(&t, group_id, 0).await?.len(), 1);
        let msg = message::Message::load_from_db(&t, msg.id).await?;
        assert_eq!(msg.state, MessageState::OutMdnRcvd);

        // check, the read-receipt has not unarchived the one2one
        assert_eq!(
            Chatlist::try_load(&t, DC_GCL_NO_SPECIALS, None, None)
                .await?
                .len(),
            1
        );
        let one2one = Chat::load_from_db(&t, one2one_id).await?;
        assert!(one2one.get_visibility() == ChatVisibility::Archived);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_no_from() {
        // if there is no from given, from_id stays 0 which is just fine. These messages
        // are very rare, however, we have to add them to the database
        // to avoid a re-download from the server.

        let t = TestContext::new_alice().await;
        let context = &t;

        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert!(chats.get_msg_id(0).is_err());

        receive_imf(
            context,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 To: bob@example.com\n\
                 Subject: foo\n\
                 Message-ID: <3924@example.com>\n\
                 Chat-Version: 1.0\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello\n",
            false,
        )
        .await
        .unwrap();

        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        // Check that the message was added to the database:
        assert!(chats.get_msg_id(0).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_escaped_from() {
        let t = TestContext::new_alice().await;
        let contact_id = Contact::create(&t, "foobar", "foobar@example.com")
            .await
            .unwrap();
        let chat_id = ChatId::create_for_contact(&t, contact_id).await.unwrap();
        receive_imf(
            &t,
            b"From: =?UTF-8?B?0JjQvNGPLCDQpNCw0LzQuNC70LjRjw==?= <foobar@example.com>\n\
                 To: alice@example.org\n\
                 Subject: foo\n\
                 Message-ID: <asdklfjjaweofi@example.com>\n\
                 Chat-Version: 1.0\n\
                 Chat-Disposition-Notification-To: =?UTF-8?B?0JjQvNGPLCDQpNCw0LzQuNC70LjRjw==?= <foobar@example.com>\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello\n",
            false,
        ).await.unwrap();
        assert_eq!(
            Contact::load_from_db(&t, contact_id)
                .await
                .unwrap()
                .get_authname(),
            "Имя, Фамилия",
        );
        let msg = get_chat_msg(&t, chat_id, 0, 1).await;
        assert_eq!(msg.is_dc_message, MessengerMessage::Yes);
        assert_eq!(msg.text.unwrap(), "hello");
        assert_eq!(msg.param.get_int(Param::WantsMdn).unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_escaped_recipients() {
        let t = TestContext::new_alice().await;
        Contact::create(&t, "foobar", "foobar@example.com")
            .await
            .unwrap();

        let carl_contact_id =
            Contact::add_or_lookup(&t, "Carl", "carl@host.tld", Origin::IncomingUnknownFrom)
                .await
                .unwrap()
                .0;

        receive_imf(
            &t,
            b"From: Foobar <foobar@example.com>\n\
                 To: =?UTF-8?B?0JjQvNGPLCDQpNCw0LzQuNC70LjRjw==?= alice@example.org\n\
                 Cc: =?utf-8?q?=3Ch2=3E?= <carl@host.tld>\n\
                 Subject: foo\n\
                 Message-ID: <asdklfjjaweofi@example.com>\n\
                 Chat-Version: 1.0\n\
                 Chat-Disposition-Notification-To: <foobar@example.com>\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello\n",
            false,
        )
        .await
        .unwrap();
        let contact = Contact::load_from_db(&t, carl_contact_id).await.unwrap();
        assert_eq!(contact.get_name(), "");
        assert_eq!(contact.get_display_name(), "h2");

        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        let msg = Message::load_from_db(&t, chats.get_msg_id(0).unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(msg.is_dc_message, MessengerMessage::Yes);
        assert_eq!(msg.text.unwrap(), "hello");
        assert_eq!(msg.param.get_int(Param::WantsMdn).unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cc_to_contact() {
        let t = TestContext::new_alice().await;
        Contact::create(&t, "foobar", "foobar@example.com")
            .await
            .unwrap();

        let carl_contact_id =
            Contact::add_or_lookup(&t, "garabage", "carl@host.tld", Origin::IncomingUnknownFrom)
                .await
                .unwrap()
                .0;

        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: Foobar <foobar@example.com>\n\
                 To: alice@example.org\n\
                 Cc: Carl <carl@host.tld>\n\
                 Subject: foo\n\
                 Message-ID: <asdklfjjaweofi@example.com>\n\
                 Chat-Version: 1.0\n\
                 Chat-Disposition-Notification-To: <foobar@example.com>\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello\n",
            false,
        )
        .await
        .unwrap();
        let contact = Contact::load_from_db(&t, carl_contact_id).await.unwrap();
        assert_eq!(contact.get_name(), "");
        assert_eq!(contact.get_display_name(), "Carl");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_tiscali() {
        test_parse_ndn(
            "alice@tiscali.it",
            "shenauithz@testrun.org",
            "Mr.un2NYERi1RM.lbQ5F9q-QyJ@tiscali.it",
            include_bytes!("../test-data/message/tiscali_ndn.eml"),
            Some("Delivery status notification –       This is an automatically generated Delivery Status Notification.      \n\nDelivery to the following recipients was aborted after 2 second(s):\n\n  * shenauithz@testrun.org"),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_testrun() {
        test_parse_ndn(
            "alice@testrun.org",
            "hcksocnsofoejx@five.chat",
            "Mr.A7pTA5IgrUA.q4bP41vAJOp@testrun.org",
            include_bytes!("../test-data/message/testrun_ndn.eml"),
            Some("Undelivered Mail Returned to Sender – This is the mail system at host hq5.merlinux.eu.\n\nI\'m sorry to have to inform you that your message could not\nbe delivered to one or more recipients. It\'s attached below.\n\nFor further assistance, please send mail to postmaster.\n\nIf you do so, please include this problem report. You can\ndelete your own text from the attached returned message.\n\n                   The mail system\n\n<hcksocnsofoejx@five.chat>: host mail.five.chat[195.62.125.103] said: 550 5.1.1\n    <hcksocnsofoejx@five.chat>: Recipient address rejected: User unknown in\n    virtual mailbox table (in reply to RCPT TO command)"),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_yahoo() {
        test_parse_ndn(
            "alice@yahoo.com",
            "haeclirth.sinoenrat@yahoo.com",
            "1680295672.3657931.1591783872936@mail.yahoo.com",
            include_bytes!("../test-data/message/yahoo_ndn.eml"),
            Some("Failure Notice – Sorry, we were unable to deliver your message to the following address.\n\n<haeclirth.sinoenrat@yahoo.com>:\n554: delivery error: dd Not a valid recipient - atlas117.free.mail.ne1.yahoo.com [...]"),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_gmail() {
        test_parse_ndn(
            "alice@gmail.com",
            "assidhfaaspocwaeofi@gmail.com",
            "CABXKi8zruXJc_6e4Dr087H5wE7sLp+u250o0N2q5DdjF_r-8wg@mail.gmail.com",
            include_bytes!("../test-data/message/gmail_ndn.eml"),
            Some("Delivery Status Notification (Failure) – ** Die Adresse wurde nicht gefunden **\n\nIhre Nachricht wurde nicht an assidhfaaspocwaeofi@gmail.com zugestellt, weil die Adresse nicht gefunden wurde oder keine E-Mails empfangen kann.\n\nHier erfahren Sie mehr: https://support.google.com/mail/?p=NoSuchUser\n\nAntwort:\n\n550 5.1.1 The email account that you tried to reach does not exist. Please try double-checking the recipient\'s email address for typos or unnecessary spaces. Learn more at https://support.google.com/mail/?p=NoSuchUser i18sor6261697wrs.38 - gsmtp"),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_gmx() {
        test_parse_ndn(
            "alice@gmx.com",
            "snaerituhaeirns@gmail.com",
            "9c9c2a32-056b-3592-c372-d7e8f0bd4bc2@gmx.de",
            include_bytes!("../test-data/message/gmx_ndn.eml"),
            Some("Mail delivery failed: returning message to sender – This message was created automatically by mail delivery software.\n\nA message that you sent could not be delivered to one or more of\nits recipients. This is a permanent error. The following address(es)\nfailed:\n\nsnaerituhaeirns@gmail.com:\nSMTP error from remote server for RCPT TO command, host: gmail-smtp-in.l.google.com (66.102.1.27) reason: 550-5.1.1 The email account that you tried to reach does not exist. Please\n try\n550-5.1.1 double-checking the recipient\'s email address for typos or\n550-5.1.1 unnecessary spaces. Learn more at\n550 5.1.1  https://support.google.com/mail/?p=NoSuchUser f6si2517766wmc.21\n9 - gsmtp [...]"),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_posteo() {
        test_parse_ndn(
            "alice@posteo.org",
            "hanerthaertidiuea@gmx.de",
            "04422840-f884-3e37-5778-8192fe22d8e1@posteo.de",
            include_bytes!("../test-data/message/posteo_ndn.eml"),
            Some("Undelivered Mail Returned to Sender – This is the mail system at host mout01.posteo.de.\n\nI\'m sorry to have to inform you that your message could not\nbe delivered to one or more recipients. It\'s attached below.\n\nFor further assistance, please send mail to postmaster.\n\nIf you do so, please include this problem report. You can\ndelete your own text from the attached returned message.\n\n                   The mail system\n\n<hanerthaertidiuea@gmx.de>: host mx01.emig.gmx.net[212.227.17.5] said: 550\n    Requested action not taken: mailbox unavailable (in reply to RCPT TO\n    command)"),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_testrun_2() {
        test_parse_ndn(
            "alice@example.org",
            "bob@example.org",
            "Mr.5xqflwt0YFv.IXDFfHauvWx@testrun.org",
            include_bytes!("../test-data/message/testrun_ndn_2.eml"),
            Some("Undelivered Mail Returned to Sender – This is the mail system at host hq5.merlinux.eu.\n\nI'm sorry to have to inform you that your message could not\nbe delivered to one or more recipients. It's attached below.\n\nFor further assistance, please send mail to postmaster.\n\nIf you do so, please include this problem report. You can\ndelete your own text from the attached returned message.\n\n                   The mail system\n\n<bob@example.org>: Host or domain name not found. Name service error for\n    name=echedelyr.tk type=AAAA: Host not found"),
        )
        .await;
    }

    /// Tests that text part is not squashed into OpenPGP attachment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_with_attachment() {
        test_parse_ndn(
            "alice@example.org",
            "bob@example.net",
            "Mr.I6Da6dXcTel.TroC5J3uSDH@example.org",
            include_bytes!("../test-data/message/ndn_with_attachment.eml"),
            Some("Undelivered Mail Returned to Sender – This is the mail system at host relay01.example.org.\n\nI'm sorry to have to inform you that your message could not\nbe delivered to one or more recipients. It's attached below.\n\nFor further assistance, please send mail to postmaster.\n\nIf you do so, please include this problem report. You can\ndelete your own text from the attached returned message.\n\n                   The mail system\n\n<bob@example.net>: host mx2.example.net[80.241.60.215] said: 552 5.2.2\n    <bob@example.net>: Recipient address rejected: Mailbox quota exceeded (in\n    reply to RCPT TO command)\n\n<bob2@example.net>: host mx1.example.net[80.241.60.212] said: 552 5.2.2\n    <bob2@example.net>: Recipient address rejected: Mailbox quota\n    exceeded (in reply to RCPT TO command)")
        )
        .await;
    }

    /// Test that DSN is not treated as NDN if Action: is not "failed"
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_dsn_relayed() {
        test_parse_ndn(
            "anon_1@posteo.de",
            "anon_2@gmx.at",
            "8b7b1a9d0c8cc588c7bcac47f5687634@posteo.de",
            include_bytes!("../test-data/message/dsn_relayed.eml"),
            None,
        )
        .await;
    }

    // ndn = Non Delivery Notification
    async fn test_parse_ndn(
        self_addr: &str,
        foreign_addr: &str,
        rfc724_mid_outgoing: &str,
        raw_ndn: &[u8],
        error_msg: Option<&str>,
    ) {
        let t = TestContext::new().await;
        t.configure_addr(self_addr).await;

        receive_imf(
            &t,
            format!(
                "Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                From: {}\n\
                To: {}\n\
                Subject: foo\n\
                Message-ID: <{}>\n\
                Chat-Version: 1.0\n\
                Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                \n\
                hello\n",
                self_addr, foreign_addr, rfc724_mid_outgoing
            )
            .as_bytes(),
            false,
        )
        .await
        .unwrap();

        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        let msg_id = chats.get_msg_id(0).unwrap().unwrap();

        // Check that the ndn would be downloaded:
        let headers = mailparse::parse_mail(raw_ndn).unwrap().headers;
        assert!(prefetch_should_download(
            &t,
            &headers,
            "some-other-message-id",
            std::iter::empty(),
            ShowEmails::Off,
        )
        .await
        .unwrap());

        receive_imf(&t, raw_ndn, false).await.unwrap();
        let msg = Message::load_from_db(&t, msg_id).await.unwrap();

        assert_eq!(
            msg.state,
            if error_msg.is_some() {
                MessageState::OutFailed
            } else {
                MessageState::OutDelivered
            }
        );

        assert_eq!(msg.error(), error_msg.map(|error| error.to_string()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_parse_ndn_group_msg() -> Result<()> {
        let t = TestContext::new().await;
        t.configure_addr("alice@gmail.com").await;

        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@gmail.com\n\
                 To: bob@example.com, assidhfaaspocwaeofi@gmail.com\n\
                 Subject: foo\n\
                 Message-ID: <CADWx9Cs32Wa7Gy-gM0bvbq54P_FEHe7UcsAV=yW7sVVW=fiMYQ@mail.gmail.com>\n\
                 Chat-Version: 1.0\n\
                 Chat-Group-ID: abcde\n\
                 Chat-Group-Name: foo\n\
                 Chat-Disposition-Notification-To: alice@example.org\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello\n",
            false,
        )
        .await?;

        let chats = Chatlist::try_load(&t, 0, None, None).await?;
        let msg_id = chats.get_msg_id(0)?.unwrap();

        let raw = include_bytes!("../test-data/message/gmail_ndn_group.eml");
        receive_imf(&t, raw, false).await?;

        let msg = Message::load_from_db(&t, msg_id).await?;

        assert_eq!(msg.state, MessageState::OutFailed);

        let msgs = chat::get_chat_msgs(&t, msg.chat_id, 0).await?;
        let msg_id = if let ChatItem::Message { msg_id } = msgs.last().unwrap() {
            msg_id
        } else {
            panic!("Wrong item type");
        };
        let last_msg = Message::load_from_db(&t, *msg_id).await?;

        assert_eq!(
            last_msg.text,
            Some(stock_str::failed_sending_to(&t, "assidhfaaspocwaeofi@gmail.com").await,)
        );
        assert_eq!(last_msg.from_id, ContactId::INFO);
        Ok(())
    }

    async fn load_imf_email(context: &Context, imf_raw: &[u8]) -> Message {
        context
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();
        receive_imf(context, imf_raw, false).await.unwrap();
        let chats = Chatlist::try_load(context, 0, None, None).await.unwrap();
        let msg_id = chats.get_msg_id(0).unwrap().unwrap();
        Message::load_from_db(context, msg_id).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_html_only_mail() {
        let t = TestContext::new_alice().await;
        let msg = load_imf_email(&t, include_bytes!("../test-data/message/wrong-html.eml")).await;
        assert_eq!(msg.text.unwrap(), "   Guten Abend,   \n\n   Lots of text   \n\n   text with Umlaut ä...   \n\n   MfG    [...]");
    }

    static GH_MAILINGLIST: &[u8] =
        b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
    From: Max Mustermann <notifications@github.com>\n\
    To: deltachat/deltachat-core-rust <deltachat-core-rust@noreply.github.com>\n\
    Subject: Let's put some [brackets here that] have nothing to do with the topic\n\
    Message-ID: <3333@example.org>\n\
    List-ID: deltachat/deltachat-core-rust <deltachat-core-rust.deltachat.github.com>\n\
    List-Post: <mailto:reply+ELERNSHSETUSHOYSESHETIHSEUSAFERUHSEDTISNEU@reply.github.com>\n\
    Precedence: list\n\
    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
    \n\
    hello\n";

    static GH_MAILINGLIST2: &str =
        "Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
    From: Github <notifications@github.com>\n\
    To: deltachat/deltachat-core-rust <deltachat-core-rust@noreply.github.com>\n\
    Subject: [deltachat/deltachat-core-rust] PR run failed\n\
    Message-ID: <3334@example.org>\n\
    List-ID: deltachat/deltachat-core-rust <deltachat-core-rust.deltachat.github.com>\n\
    List-Post: <mailto:reply+EGELITBABIHXSITUZIEPAKYONASITEPUANERGRUSHE@reply.github.com>\n\
    Precedence: list\n\
    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
    \n\
    hello back\n";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_github_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.ctx.set_config(Config::ShowEmails, Some("2")).await?;

        receive_imf(&t.ctx, GH_MAILINGLIST, false).await?;

        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await?;
        assert_eq!(chats.len(), 1);

        let chat_id = chats.get_chat_id(0).unwrap();
        chat_id.accept(&t).await.unwrap();
        let chat = chat::Chat::load_from_db(&t.ctx, chat_id).await?;

        assert!(chat.is_mailing_list());
        assert!(chat.can_send(&t.ctx).await?);
        assert_eq!(
            chat.get_mailinglist_addr(),
            "reply+elernshsetushoyseshetihseusaferuhsedtisneu@reply.github.com"
        );
        assert_eq!(chat.name, "deltachat/deltachat-core-rust");
        assert_eq!(chat::get_chat_contacts(&t.ctx, chat_id).await?.len(), 1);

        receive_imf(&t.ctx, GH_MAILINGLIST2.as_bytes(), false).await?;

        let chat = chat::Chat::load_from_db(&t.ctx, chat_id).await?;
        assert!(!chat.can_send(&t.ctx).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await?;
        assert_eq!(chats.len(), 1);
        let contacts = Contact::get_all(&t.ctx, 0, None).await?;
        assert_eq!(contacts.len(), 0); // mailing list recipients and senders do not count as "known contacts"

        let msg1 = get_chat_msg(&t, chat_id, 0, 2).await;
        let contact1 = Contact::load_from_db(&t.ctx, msg1.from_id).await?;
        assert_eq!(contact1.get_addr(), "notifications@github.com");
        assert_eq!(contact1.get_display_name(), "notifications@github.com"); // Make sure this is not "Max Mustermann" or somethinng

        let msg2 = get_chat_msg(&t, chat_id, 1, 2).await;
        let contact2 = Contact::load_from_db(&t.ctx, msg2.from_id).await?;
        assert_eq!(contact2.get_addr(), "notifications@github.com");

        assert_eq!(msg1.get_override_sender_name().unwrap(), "Max Mustermann");
        assert_eq!(msg2.get_override_sender_name().unwrap(), "Github");
        Ok(())
    }

    static DC_MAILINGLIST: &[u8] = b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
    From: Bob <bob@posteo.org>\n\
    To: delta@codespeak.net\n\
    Subject: Re: [delta-dev] What's up?\n\
    Message-ID: <38942@posteo.org>\n\
    List-ID: \"discussions about and around https://delta.chat developments\" <delta.codespeak.net>\n\
    List-Post: <mailto:delta@codespeak.net>\n\
    Precedence: list\n\
    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
    \n\
    body\n";

    static DC_MAILINGLIST2: &[u8] = b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
    From: Charlie <charlie@posteo.org>\n\
    To: delta@codespeak.net\n\
    Subject: Re: [delta-dev] DC is nice!\n\
    Message-ID: <38943@posteo.org>\n\
    List-ID: \"discussions about and around https://delta.chat developments\" <delta.codespeak.net>\n\
    List-Post: <mailto:delta@codespeak.net>\n\
    Precedence: list\n\
    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
    \n\
    body 4\n";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_classic_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.ctx
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();
        receive_imf(&t.ctx, DC_MAILINGLIST, false).await.unwrap();
        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        let chat_id = chats.get_chat_id(0).unwrap();
        chat_id.accept(&t).await.unwrap();
        let chat = Chat::load_from_db(&t.ctx, chat_id).await.unwrap();
        assert_eq!(chat.name, "delta-dev");
        assert!(chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "delta@codespeak.net");

        let msg = get_chat_msg(&t, chat_id, 0, 1).await;
        let contact1 = Contact::load_from_db(&t.ctx, msg.from_id).await.unwrap();
        assert_eq!(contact1.get_addr(), "bob@posteo.org");

        let sent = t.send_text(chat.id, "Hello mailinglist!").await;
        let mime = sent.payload();

        println!("Sent mime message is:\n\n{}\n\n", mime);
        assert!(
            mime.contains("Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no\r\n")
        );
        assert!(mime.contains("Subject: =?utf-8?q?Re=3A_=5Bdelta-dev=5D_What=27s_up=3F?=\r\n"));
        assert!(mime.contains("MIME-Version: 1.0\r\n"));
        assert!(mime.contains("In-Reply-To: <38942@posteo.org>\r\n"));
        assert!(mime.contains("Chat-Version: 1.0\r\n"));
        assert!(mime.contains("To: <delta@codespeak.net>\r\n"));
        assert!(mime.contains("From: <alice@example.org>\r\n"));
        assert!(mime.contains(
            "\r\n\
\r\n\
Hello mailinglist!\r\n"
        ));

        receive_imf(&t.ctx, DC_MAILINGLIST2, false).await?;

        let chat = chat::Chat::load_from_db(&t.ctx, chat_id).await?;
        assert!(chat.can_send(&t.ctx).await?);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_other_device_writes_to_mailinglist() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;
        receive_imf(&t, DC_MAILINGLIST, false).await.unwrap();
        let first_msg = t.get_last_msg().await;
        let first_chat = Chat::load_from_db(&t, first_msg.chat_id).await?;
        assert_eq!(
            first_chat.param.get(Param::ListPost).unwrap(),
            "delta@codespeak.net"
        );

        let list_post_contact_id =
            Contact::lookup_id_by_addr(&t, "delta@codespeak.net", Origin::Unknown)
                .await?
                .unwrap();
        let list_post_contact = Contact::load_from_db(&t, list_post_contact_id).await?;
        assert_eq!(
            list_post_contact.param.get(Param::ListId).unwrap(),
            "delta.codespeak.net"
        );
        assert_eq!(
            chat::get_chat_id_by_grpid(&t, "delta.codespeak.net")
                .await?
                .unwrap(),
            (first_chat.id, false, Blocked::Request)
        );

        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
            From: Alice <alice@example.org>\n\
            To: delta@codespeak.net\n\
            Subject: [delta-dev] Subject\n\
            Message-ID: <0476@example.org>\n\
            Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
            \n\
            body 4\n",
            false,
        )
        .await
        .unwrap();

        let second_msg = t.get_last_msg().await;

        assert_eq!(first_msg.chat_id, second_msg.chat_id);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_block_mailing_list() {
        let t = TestContext::new_alice().await;
        t.ctx
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();

        receive_imf(&t.ctx, DC_MAILINGLIST, false).await.unwrap();
        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);
        let chat_id = chats.get_chat_id(0).unwrap();
        let chat = Chat::load_from_db(&t.ctx, chat_id).await.unwrap();
        assert!(chat.is_contact_request());

        // Block the contact request.
        chat_id.block(&t).await.unwrap();

        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0); // Test that the message disappeared

        receive_imf(&t.ctx, DC_MAILINGLIST2, false).await.unwrap();

        // Test that the mailing list stays disappeared
        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0); // Test that the message is not shown

        // Both messages are in the same blocked chat.
        let msgs = chat::get_chat_msgs(&t.ctx, chat_id, 0).await.unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailing_list_decide_block_then_unblock() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(&t, DC_MAILINGLIST, false).await.unwrap();
        let blocked = Contact::get_all_blocked(&t).await.unwrap();
        assert_eq!(blocked.len(), 0);

        // Block the contact request, this should add one blocked contact.
        let msg = t.get_last_msg().await;
        msg.chat_id.block(&t).await.unwrap();

        let blocked = Contact::get_all_blocked(&t).await.unwrap();
        assert_eq!(blocked.len(), 1);
        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0); // Test that the message is not shown

        // Unblock contact and check if the next message arrives in a chat
        Contact::unblock(&t, *blocked.first().unwrap())
            .await
            .unwrap();
        let blocked = Contact::get_all_blocked(&t).await.unwrap();
        assert_eq!(blocked.len(), 0);

        receive_imf(&t.ctx, DC_MAILINGLIST2, false).await.unwrap();
        let msg = t.get_last_msg().await;
        let msgs = chat::get_chat_msgs(&t, msg.chat_id, 0).await.unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailing_list_decide_not_now() {
        let t = TestContext::new_alice().await;
        t.ctx
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();

        receive_imf(&t.ctx, DC_MAILINGLIST, false).await.unwrap();

        let msg = t.get_last_msg().await;
        let chat_id = msg.get_chat_id();

        // Open the chat and go back
        chat::marknoticed_chat(&t.ctx, chat_id).await.unwrap();

        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1); // Test that chat is still in the chatlist
        let msgs = chat::get_chat_msgs(&t.ctx, chat_id, 0).await.unwrap();
        assert_eq!(msgs.len(), 1); // ...and contains 1 message

        receive_imf(&t.ctx, DC_MAILINGLIST2, false).await.unwrap();

        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1); // Test that the new mailing list message got into the same chat
        let msgs = chat::get_chat_msgs(&t.ctx, chat_id, 0).await.unwrap();
        assert_eq!(msgs.len(), 2);
        let chat = Chat::load_from_db(&t.ctx, chat_id).await.unwrap();
        assert!(chat.is_contact_request());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailing_list_decide_accept() {
        let t = TestContext::new_alice().await;
        t.ctx
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();

        receive_imf(&t.ctx, DC_MAILINGLIST, false).await.unwrap();

        let msg = t.get_last_msg().await;
        let chat_id = msg.get_chat_id();
        chat_id.accept(&t).await.unwrap();

        let chats = Chatlist::try_load(&t.ctx, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1); // Test that the message is shown
        assert!(!chat_id.is_special());

        receive_imf(&t.ctx, DC_MAILINGLIST2, false).await.unwrap();

        let msgs = chat::get_chat_msgs(&t.ctx, chat_id, 0).await.unwrap();
        assert_eq!(msgs.len(), 2);
        let chat = chat::Chat::load_from_db(&t.ctx, chat_id).await.unwrap();
        assert!(chat.can_send(&t.ctx).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailing_list_multiple_names_in_subject() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;
        receive_imf(
            &t,
            b"From: Foo Bar <foo@bar.org>\n\
    To: deltachat/deltachat-core-rust <deltachat-core-rust@noreply.github.com>\n\
    Subject: [ola list] [foo][bar]  just a subject\n\
    Message-ID: <3333@example.org>\n\
    List-ID: \"looong description of 'ola list', with foo, bar\" <delta.codespeak.net>\n\
    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
    \n\
    hello\n",
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        let chat_id = msg.get_chat_id();
        let chat = Chat::load_from_db(&t, chat_id).await?;
        assert_eq!(chat.name, "ola list [foo][bar]");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_majordomo_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        // test mailing lists not having a `ListId:`-header
        receive_imf(
            &t,
            b"From: Foo Bar <foo@bar.org>\n\
    To: deltachat/deltachat-core-rust <deltachat-core-rust@noreply.github.com>\n\
    Subject: [ola] just a subject\n\
    Message-ID: <3333@example.org>\n\
    Sender: My list <mylist@bar.org>\n\
    Precedence: list\n\
    Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
    \n\
    hello\n",
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        let chat_id = msg.get_chat_id();
        let chat = Chat::load_from_db(&t, chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.grpid, "mylist@bar.org");
        assert_eq!(chat.name, "ola");
        assert_eq!(chat::get_chat_msgs(&t, chat.id, 0).await.unwrap().len(), 1);
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        // receive another message with no sender name but the same address,
        // make sure this lands in the same chat
        receive_imf(
            &t,
            b"From: Nu Bar <nu@bar.org>\n\
    To: deltachat/deltachat-core-rust <deltachat-core-rust@noreply.github.com>\n\
    Subject: [ola] Re: just a subject\n\
    Message-ID: <4444@example.org>\n\
    Sender: mylist@bar.org\n\
    Precedence: list\n\
    Date: Sun, 22 Mar 2020 23:37:57 +0000\n\
    \n\
    hello\n",
            false,
        )
        .await
        .unwrap();
        assert_eq!(chat::get_chat_msgs(&t, chat.id, 0).await.unwrap().len(), 2);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailchimp_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(
            &t,
            b"To: alice <alice@example.org>\n\
            Subject: =?utf-8?Q?How=20early=20megacities=20emerged=20from=20Cambodia=E2=80=99s=20jungles?=\n\
            From: =?utf-8?Q?Atlas=20Obscura?= <info@atlasobscura.com>\n\
            List-ID: 399fc0402f1b154b67965632emc list <399fc0402f1b154b67965632e.100761.list-id.mcsv.net>\n\
            Message-ID: <555@example.org>\n\
            Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
            \n\
            hello\n",
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.blocked, Blocked::Request);
        assert_eq!(
            chat.grpid,
            "399fc0402f1b154b67965632e.100761.list-id.mcsv.net"
        );
        assert_eq!(chat.name, "Atlas Obscura");
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dhl_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_dhl.eml"),
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        assert_eq!(
            msg.text,
            Some("Ihr Paket ist in der Packstation 123 – bla bla".to_string())
        );
        assert!(msg.has_html());
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.blocked, Blocked::Request);
        assert_eq!(chat.grpid, "1234ABCD-123LMNO.mailing.dhl.de");
        assert_eq!(chat.name, "DHL Paket");
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dpd_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_dpd.eml"),
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        assert_eq!(
            msg.text,
            Some("Bald ist Ihr DPD Paket da – bla bla".to_string())
        );
        assert!(msg.has_html());
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.blocked, Blocked::Request);
        assert_eq!(chat.grpid, "dpdde.mxmail.service.dpd.de");
        assert_eq!(chat.name, "DPD");
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_xt_local_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_xt_local_microsoft.eml"),
            false,
        )
        .await?;
        let chat = Chat::load_from_db(&t, t.get_last_msg().await.chat_id).await?;
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.grpid, "96540.xt.local");
        assert_eq!(chat.name, "Microsoft Store");
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_xt_local_spiegel.eml"),
            false,
        )
        .await?;
        let chat = Chat::load_from_db(&t, t.get_last_msg().await.chat_id).await?;
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.grpid, "121231234.xt.local");
        assert_eq!(chat.name, "DER SPIEGEL Kundenservice");
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_xing_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_xing.eml"),
            false,
        )
        .await?;
        let msg = t.get_last_msg().await;
        assert_eq!(msg.subject, "Kennst Du Dr. Mabuse?");
        let chat = Chat::load_from_db(&t, msg.chat_id).await?;
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.grpid, "51231231231231231231231232869f58.xing.com");
        assert_eq!(chat.name, "xing.com");
        assert!(!chat.can_send(&t).await?);
        assert_eq!(chat.get_mailinglist_addr(), "");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ttline_mailing_list() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_ttline.eml"),
            false,
        )
        .await?;
        let msg = t.get_last_msg().await;
        assert_eq!(msg.subject, "Unsere Sommerangebote an Bord ⚓");
        let chat = Chat::load_from_db(&t, msg.chat_id).await?;
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.grpid, "39123123-1BBQXPY.t.ttline.com");
        assert_eq!(chat.name, "TT-Line - Die Schwedenfähren");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailing_list_with_mimepart_footer() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        // the mailing list message contains two top-level texts.
        // the second text is a footer that is added by some mailing list software
        // if the user-edited text contains html.
        // this footer should not become a text-message in delta chat
        // (otherwise every second mail might be the same footer)
        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_with_mimepart_footer.eml"),
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        assert_eq!(
            msg.text,
            Some("[Intern] important stuff – Hi mr ... [text part]".to_string())
        );
        assert!(msg.has_html());
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(get_chat_msgs(&t, msg.chat_id, 0).await.unwrap().len(), 1);
        assert_eq!(chat.typ, Chattype::Mailinglist);
        assert_eq!(chat.blocked, Blocked::Request);
        assert_eq!(chat.grpid, "intern.lists.abc.de");
        assert_eq!(chat.name, "Intern");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mailing_list_with_mimepart_footer_signed() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(
            &t,
            include_bytes!("../test-data/message/mailinglist_with_mimepart_footer_signed.eml"),
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        assert_eq!(get_chat_msgs(&t, msg.chat_id, 0).await.unwrap().len(), 1);
        let text = msg.text.clone().unwrap();
        assert!(text.contains("content text"));
        assert!(!text.contains("footer text"));
        assert!(msg.has_html());
        let html = msg.get_id().get_html(&t).await.unwrap().unwrap();
        assert!(html.contains("content text"));
        assert!(!html.contains("footer text"));
    }

    /// Test that the changes from apply_mailinglist_changes() are also applied
    /// if the message is assigned to the chat by In-Reply-To
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apply_mailinglist_changes_assigned_by_reply() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(&t, GH_MAILINGLIST, false).await.unwrap();

        let chat_id = t.get_last_msg().await.chat_id;
        chat_id.accept(&t).await.unwrap();
        let chat = Chat::load_from_db(&t, chat_id).await.unwrap();
        assert!(chat.can_send(&t).await.unwrap());

        let imf_raw = format!("In-Reply-To: 3333@example.org\n{}", GH_MAILINGLIST2);
        receive_imf(&t, imf_raw.as_bytes(), false).await.unwrap();

        assert_eq!(
            t.get_last_msg().await.in_reply_to.unwrap(),
            "3333@example.org"
        );
        // `Assigning message to Chat#... as it's a reply to 3333@example.org`
        t.evtracker
            .get_info_contains("as it's a reply to 3333@example.org")
            .await;

        let chat = Chat::load_from_db(&t, chat_id).await.unwrap();
        assert!(!chat.can_send(&t).await.unwrap());

        let contact_id = Contact::lookup_id_by_addr(
            &t,
            "reply+EGELITBABIHXSITUZIEPAKYONASITEPUANERGRUSHE@reply.github.com",
            Origin::Hidden,
        )
        .await
        .unwrap()
        .unwrap();
        let contact = Contact::load_from_db(&t, contact_id).await.unwrap();
        assert_eq!(
            contact.param.get(Param::ListId).unwrap(),
            "deltachat-core-rust.deltachat.github.com"
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dont_show_tokens_in_contacts_list() {
        check_dont_show_in_contacts_list(
            "reply+OGHVYCLVBEGATYBICAXBIRQATABUOTUCERABERAHNO@reply.github.com",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dont_show_noreply_in_contacts_list() {
        check_dont_show_in_contacts_list("noreply@github.com").await;
    }

    async fn check_dont_show_in_contacts_list(addr: &str) {
        let t = TestContext::new_alice().await;
        t.ctx
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();
        receive_imf(
            &t,
            format!(
                "Subject: Re: [deltachat/deltachat-core-rust] DC is the best repo on GitHub!
To: {}
References: <deltachat/deltachat-core-rust/pull/1625@github.com>
 <deltachat/deltachat-core-rust/pull/1625/c644661857@github.com>
From: alice@example.org
Message-ID: <d2717387-0ba7-7b60-9b09-fd89a76ea8a0@gmx.de>
Date: Tue, 16 Jun 2020 12:04:20 +0200
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: 7bit

YEAAAAAA!.
",
                addr
            )
            .as_bytes(),
            false,
        )
        .await
        .unwrap();
        let contacts = Contact::get_all(&t, 0, None as Option<&str>).await.unwrap();
        assert!(contacts.is_empty()); // The contact should not have been added to the db
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pdf_filename_simple() {
        let t = TestContext::new_alice().await;
        let msg = load_imf_email(
            &t,
            include_bytes!("../test-data/message/pdf_filename_simple.eml"),
        )
        .await;
        assert_eq!(msg.viewtype, Viewtype::File);
        assert_eq!(msg.text.unwrap(), "mail body");
        assert_eq!(msg.param.get(Param::File).unwrap(), "$BLOBDIR/simple.pdf");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pdf_filename_continuation() {
        // test filenames split across multiple header lines, see rfc 2231
        let t = TestContext::new_alice().await;
        let msg = load_imf_email(
            &t,
            include_bytes!("../test-data/message/pdf_filename_continuation.eml"),
        )
        .await;
        assert_eq!(msg.viewtype, Viewtype::File);
        assert_eq!(msg.text.unwrap(), "mail body");
        assert_eq!(
            msg.param.get(Param::File).unwrap(),
            "$BLOBDIR/test pdf äöüß.pdf"
        );
    }

    /// HTML-images may come with many embedded images, eg. tiny icons, corners for formatting,
    /// twitter/facebook/whatever logos and so on.
    /// that may easily be 50 and more images, one would not have these images in a chat.
    ///
    /// fortunately, if we remove them, they are accessible by get_msg_html() now.
    ///
    /// unfortunately, these images are not that easy to detect as they may also be on purpose,
    /// or mua may use multipart/related not correctly -
    /// so this test is in competition with parse_thunderbird_html_embedded_image()
    /// that wants the image to be kept in the chat.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_many_images() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

        receive_imf(
            &t,
            include_bytes!("../test-data/message/many_images_amazon_via_apple_mail.eml"),
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        assert_eq!(msg.viewtype, Viewtype::Image);
        assert!(msg.has_html());
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(get_chat_msgs(&t, chat.id, 0).await.unwrap().len(), 1);
    }

    /// Test that classical MUA messages are assigned to group chats based on the `In-Reply-To`
    /// header.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_in_reply_to() {
        let t = TestContext::new().await;
        t.configure_addr("bob@example.com").await;

        // Receive message from Alice about group "foo".
        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@example.org\n\
                 To: bob@example.com, charlie@example.net\n\
                 Subject: foo\n\
                 Message-ID: <message@example.org>\n\
                 Chat-Version: 1.0\n\
                 Chat-Group-ID: foo\n\
                 Chat-Group-Name: foo\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello foo\n",
            false,
        )
        .await
        .unwrap();

        // Receive reply from Charlie without group ID but with In-Reply-To header.
        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: charlie@example.net\n\
                 To: alice@example.org, bob@example.com\n\
                 Subject: Re: foo\n\
                 Message-ID: <message@example.net>\n\
                 In-Reply-To: <message@example.org>\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 reply foo\n",
            false,
        )
        .await
        .unwrap();

        let msg = t.get_last_msg().await;
        assert_eq!(msg.get_text().unwrap(), "reply foo");

        // Load the first message from the same chat.
        let msgs = chat::get_chat_msgs(&t, msg.chat_id, 0).await.unwrap();
        let msg_id = if let ChatItem::Message { msg_id } = msgs.first().unwrap() {
            msg_id
        } else {
            panic!("Wrong item type");
        };

        let reply_msg = Message::load_from_db(&t, *msg_id).await.unwrap();
        assert_eq!(reply_msg.get_text().unwrap(), "hello foo");

        // Check that reply got into the same chat as the original message.
        assert_eq!(msg.chat_id, reply_msg.chat_id);

        // Make sure we looked at real chat ID and do not just
        // test that both messages got into the same virtual chat.
        assert!(!msg.chat_id.is_special());
    }

    /// Test that classical MUA messages are assigned to group chats
    /// based on the `In-Reply-To` header for two-member groups.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_in_reply_to_two_member_group() {
        let t = TestContext::new().await;
        t.configure_addr("bob@example.com").await;

        // Receive message from Alice about group "foo".
        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@example.org\n\
                 To: bob@example.com\n\
                 Subject: foo\n\
                 Message-ID: <message@example.org>\n\
                 Chat-Version: 1.0\n\
                 Chat-Group-ID: foo\n\
                 Chat-Group-Name: foo\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 hello foo\n",
            false,
        )
        .await
        .unwrap();

        // Receive a classic MUA reply from Alice.
        // It is assigned to the group chat.
        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@example.org\n\
                 To: bob@example.com\n\
                 Subject: Re: foo\n\
                 Message-ID: <reply@example.org>\n\
                 In-Reply-To: <message@example.org>\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 classic reply\n",
            false,
        )
        .await
        .unwrap();

        // Ensure message is assigned to group chat.
        let msg = t.get_last_msg().await;
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Group);
        assert_eq!(msg.get_text().unwrap(), "classic reply");

        // Receive a Delta Chat reply from Alice.
        // It is assigned to group chat, because it has a group ID.
        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@example.org\n\
                 To: bob@example.com\n\
                 Subject: Re: foo\n\
                 Message-ID: <chatreply@example.org>\n\
                 In-Reply-To: <message@example.org>\n\
                 Chat-Version: 1.0\n\
                 Chat-Group-ID: foo\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 chat reply\n",
            false,
        )
        .await
        .unwrap();

        // Ensure message is assigned to group chat.
        let msg = t.get_last_msg().await;
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Group);
        assert_eq!(msg.get_text().unwrap(), "chat reply");

        // Receive a private Delta Chat reply from Alice.
        // It is assigned to 1:1 chat, because it has no group ID,
        // which means it was created using "reply privately" feature.
        // Normally it contains a quote, but it should not matter.
        receive_imf(
            &t,
            b"Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                 From: alice@example.org\n\
                 To: bob@example.com\n\
                 Subject: Re: foo\n\
                 Message-ID: <chatprivatereply@example.org>\n\
                 In-Reply-To: <message@example.org>\n\
                 Chat-Version: 1.0\n\
                 Date: Sun, 22 Mar 2020 22:37:57 +0000\n\
                 \n\
                 private reply\n",
            false,
        )
        .await
        .unwrap();

        // Ensure message is assigned to a 1:1 chat.
        let msg = t.get_last_msg().await;
        let chat = Chat::load_from_db(&t, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Single);
        assert_eq!(msg.get_text().unwrap(), "private reply");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_save_mime_headers_off() -> anyhow::Result<()> {
        let alice = TestContext::new_alice().await;
        let bob = TestContext::new_bob().await;
        let chat_alice = alice.create_chat(&bob).await;
        chat::send_text_msg(&alice, chat_alice.id, "hi!".to_string()).await?;

        let msg = bob.recv_msg(&alice.pop_sent_msg().await).await;
        assert_eq!(msg.get_text(), Some("hi!".to_string()));
        assert!(!msg.get_showpadlock());
        let mime = message::get_mime_headers(&bob, msg.id).await?;
        assert!(mime.is_empty());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_save_mime_headers_on() -> anyhow::Result<()> {
        let alice = TestContext::new_alice().await;
        alice.set_config_bool(Config::SaveMimeHeaders, true).await?;
        let bob = TestContext::new_bob().await;
        bob.set_config_bool(Config::SaveMimeHeaders, true).await?;

        // alice sends a message to bob, bob sees full mime
        let chat_alice = alice.create_chat(&bob).await;
        chat::send_text_msg(&alice, chat_alice.id, "hi!".to_string()).await?;

        let msg = bob.recv_msg(&alice.pop_sent_msg().await).await;
        assert_eq!(msg.get_text(), Some("hi!".to_string()));
        assert!(!msg.get_showpadlock());
        let mime = message::get_mime_headers(&bob, msg.id).await?;
        let mime_str = String::from_utf8_lossy(&mime);
        assert!(mime_str.contains("Message-ID:"));
        assert!(mime_str.contains("From:"));

        // another one, from bob to alice, that gets encrypted
        let chat_bob = bob.create_chat(&alice).await;
        chat::send_text_msg(&bob, chat_bob.id, "ho!".to_string()).await?;
        let msg = alice.recv_msg(&bob.pop_sent_msg().await).await;
        assert_eq!(msg.get_text(), Some("ho!".to_string()));
        assert!(msg.get_showpadlock());
        let mime = message::get_mime_headers(&alice, msg.id).await?;
        let mime_str = String::from_utf8_lossy(&mime);
        assert!(mime_str.contains("Message-ID:"));
        assert!(mime_str.contains("From:"));
        Ok(())
    }

    async fn create_test_alias(
        chat_request: bool,
        group_request: bool,
    ) -> (TestContext, TestContext) {
        // Claire, a customer, sends a support request
        // to the alias address <support@example.org> from a classic MUA.
        // The alias expands to the supporters Alice and Bob.
        // Check that Alice receives the message in a group chat.
        let claire_request = if group_request {
            format!(
                "Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                To: support@example.org, ceo@example.org\n\
                From: claire@example.org\n\
                Subject: i have a question\n\
                Message-ID: <non-dc-1@example.org>\n\
                {}\
                Date: Sun, 14 Mar 2021 17:04:36 +0100\n\
                Content-Type: text/plain\n\
                \n\
                hi support! what is the current version?",
                if chat_request {
                    "Chat-Group-ID: 8ud29aridt29arid\n\
                    Chat-Group-Name: =?utf-8?q?i_have_a_question?=\n"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "Received: (Postfix, from userid 1000); Mon, 4 Dec 2006 14:51:39 +0100 (CET)\n\
                To: support@example.org\n\
                From: claire@example.org\n\
                Subject: i have a question\n\
                Message-ID: <non-dc-1@example.org>\n\
                {}\
                Date: Sun, 14 Mar 2021 17:04:36 +0100\n\
                Content-Type: text/plain\n\
                \n\
                hi support! what is the current version?",
                if chat_request {
                    "Chat-Version: 1.0\n"
                } else {
                    ""
                }
            )
        };

        let alice = TestContext::new_alice().await;
        alice
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();
        receive_imf(&alice, claire_request.as_bytes(), false)
            .await
            .unwrap();

        let msg = alice.get_last_msg().await;
        assert_eq!(msg.get_subject(), "i have a question");
        assert!(msg.get_text().unwrap().contains("hi support!"));
        let chat = Chat::load_from_db(&alice, msg.chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Group);
        assert_eq!(get_chat_msgs(&alice, chat.id, 0).await.unwrap().len(), 1);
        if group_request {
            assert_eq!(get_chat_contacts(&alice, chat.id).await.unwrap().len(), 4);
        } else {
            assert_eq!(get_chat_contacts(&alice, chat.id).await.unwrap().len(), 3);
        }
        assert_eq!(msg.get_override_sender_name(), None);

        let claire = TestContext::new().await;
        claire.configure_addr("claire@example.org").await;
        claire
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();
        receive_imf(&claire, claire_request.as_bytes(), false)
            .await
            .unwrap();

        let msg_id = rfc724_mid_exists(&claire, "non-dc-1@example.org")
            .await
            .unwrap()
            .unwrap();

        let msg = Message::load_from_db(&claire, msg_id).await.unwrap();
        msg.chat_id.accept(&claire).await.unwrap();
        assert_eq!(msg.get_subject(), "i have a question");
        assert!(msg.get_text().unwrap().contains("hi support!"));
        let chat = Chat::load_from_db(&claire, msg.chat_id).await.unwrap();
        if group_request {
            assert_eq!(chat.typ, Chattype::Group);
        } else {
            assert_eq!(chat.typ, Chattype::Single);
        }
        assert_eq!(get_chat_msgs(&claire, chat.id, 0).await.unwrap().len(), 1);
        assert_eq!(msg.get_override_sender_name(), None);

        (claire, alice)
    }

    async fn check_alias_reply(reply: &[u8], chat_request: bool, group_request: bool) {
        let (claire, alice) = create_test_alias(chat_request, group_request).await;

        // Check that Alice gets the message in the same chat.
        let request = alice.get_last_msg().await;
        receive_imf(&alice, reply, false).await.unwrap();
        let answer = alice.get_last_msg().await;
        assert_eq!(answer.get_subject(), "Re: i have a question");
        assert!(answer.get_text().unwrap().contains("the version is 1.0"));
        assert_eq!(answer.chat_id, request.chat_id);
        let chat_contacts = get_chat_contacts(&alice, answer.chat_id)
            .await
            .unwrap()
            .len();
        if group_request {
            // Claire, Support, CEO and Alice (Bob is not added)
            assert_eq!(chat_contacts, 4);
        } else {
            // Claire, Support and Alice
            assert_eq!(chat_contacts, 3);
        }
        assert_eq!(
            answer.get_override_sender_name().unwrap(),
            "bob@example.net"
        ); // Bob is not part of the group, so override-sender-name should be set

        // Check that Claire also gets the message in the same chat.
        let request = claire.get_last_msg().await;
        receive_imf(&claire, reply, false).await.unwrap();
        let answer = claire.get_last_msg().await;
        assert_eq!(answer.get_subject(), "Re: i have a question");
        assert!(answer.get_text().unwrap().contains("the version is 1.0"));
        assert_eq!(answer.chat_id, request.chat_id);
        assert_eq!(
            answer.get_override_sender_name().unwrap(),
            "bob@example.net"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_alias_support_answer_from_nondc() {
        // Bob, the other supporter, answers with a classic MUA.
        let bob_answer = b"To: support@example.org, claire@example.org\n\
        From: bob@example.net\n\
        Subject: =?utf-8?q?Re=3A_i_have_a_question?=\n\
        References: <non-dc-1@example.org>\n\
        In-Reply-To: <non-dc-1@example.org>\n\
        Message-ID: <non-dc-2@example.net>\n\
        Date: Sun, 14 Mar 2021 16:04:57 +0000\n\
        Content-Type: text/plain\n\
        \n\
        hi claire, the version is 1.0, cheers bob";

        check_alias_reply(bob_answer, true, true).await;
        check_alias_reply(bob_answer, false, true).await;
        check_alias_reply(bob_answer, true, false).await;
        check_alias_reply(bob_answer, false, false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_alias_answer_from_dc() {
        // Bob, the other supporter, answers with Delta Chat.
        let bob_answer = b"To: support@example.org, claire@example.org\n\
                From: bob@example.net\n\
                Subject: =?utf-8?q?Re=3A_i_have_a_question?=\n\
                References: <Gr.af9e810c9b592927.gNm8dVdkZsH@example.net>\n\
                In-Reply-To: <non-dc-1@example.org>\n\
                Message-ID: <Gr.af9e810c9b592927.gNm8dVdkZsH@example.net>\n\
                Date: Sun, 14 Mar 2021 16:04:57 +0000\n\
                Chat-Version: 1.0\n\
                Chat-Group-ID: af9e810c9b592927\n\
                Chat-Group-Name: =?utf-8?q?i_have_a_question?=\n\
                Chat-Disposition-Notification-To: bob@example.net\n\
                Content-Type: text/plain\n\
                \n\
                hi claire, the version is 1.0, cheers bob";

        check_alias_reply(bob_answer, true, true).await;
        check_alias_reply(bob_answer, false, true).await;
        check_alias_reply(bob_answer, true, false).await;
        check_alias_reply(bob_answer, false, false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dont_assign_to_trash_by_parent() {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await.unwrap();
        println!("\n========= Receive a message ==========");
        receive_imf(
            &t,
            b"From: Nu Bar <nu@bar.org>\n\
            To: alice@example.org, bob@example.org\n\
            Subject: Hi\n\
            Message-ID: <4444@example.org>\n\
            \n\
            hello\n",
            false,
        )
        .await
        .unwrap();
        let chat_id = t.get_last_msg().await.chat_id;
        chat_id.accept(&t).await.unwrap();
        let msg = get_chat_msg(&t, chat_id, 0, 1).await; // Make sure that the message is actually in the chat
        assert!(!msg.chat_id.is_special());
        assert_eq!(msg.text.unwrap(), "Hi – hello");

        println!("\n========= Delete the message ==========");
        msg.id.trash(&t).await.unwrap();

        let msgs = chat::get_chat_msgs(&t.ctx, chat_id, 0).await.unwrap();
        assert_eq!(msgs.len(), 0);

        println!("\n========= Receive a message that is a reply to the deleted message ==========");
        receive_imf(
            &t,
            b"From: Nu Bar <nu@bar.org>\n\
            To: alice@example.org, bob@example.org\n\
            Subject: Re: Hi\n\
            Message-ID: <5555@example.org>\n\
            In-Reply-To: <4444@example.org\n\
            \n\
            Reply\n",
            false,
        )
        .await
        .unwrap();
        let msg = t.get_last_msg().await;
        assert!(!msg.chat_id.is_special()); // Esp. check that the chat_id is not TRASH
        assert_eq!(msg.text.unwrap(), "Reply");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dont_show_all_outgoing_msgs_in_self_chat() {
        // Regression test for <https://github.com/deltachat/deltachat-android/issues/1940>:
        // Some servers add a `Bcc: <Self>` header, which caused all outgoing messages to
        // be shown in the self-chat.
        let t = TestContext::new_alice().await;

        receive_imf(
            &t,
            b"Bcc: alice@example.org
Received: from [127.0.0.1]
Subject: s
Chat-Version: 1.0
Message-ID: <abcd@gmail.com>
To: <me@other.maildomain.com>
From: <alice@example.org>

Message content",
            false,
        )
        .await
        .unwrap();

        let msg = t.get_last_msg().await;
        assert_ne!(msg.chat_id, t.get_self_chat().await.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_outgoing_classic_mail_creates_chat() {
        let alice = TestContext::new_alice().await;

        // Alice enables classic emails.
        alice
            .set_config(Config::ShowEmails, Some("2"))
            .await
            .unwrap();

        // Alice downloads outgoing classic email.
        receive_imf(
            &alice,
            b"Received: from [127.0.0.1]
Subject: Subj
Message-ID: <abcd@example.com>
To: <bob@example.org>
From: <alice@example.org>

Message content",
            false,
        )
        .await
        .unwrap();

        // Outgoing email should create a chat.
        let msg = alice.get_last_msg().await;
        assert_eq!(msg.get_text().unwrap(), "Subj – Message content");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_duplicate_message() -> Result<()> {
        // Test that duplicate messages are ignored based on the Message-ID
        let alice = TestContext::new_alice().await;

        let bob_contact_id = Contact::add_or_lookup(
            &alice,
            "Bob",
            "bob@example.org",
            Origin::IncomingUnknownFrom,
        )
        .await?
        .0;

        let first_message = b"Received: from [127.0.0.1]
Subject: First message
Message-ID: <first@example.org>
To: Alice <alice@example.org>
From: Bob1 <bob@example.org>
Chat-Version: 1.0

Message content

-- 
First signature";

        let second_message = b"Received: from [127.0.0.1]
Subject: Second message
Message-ID: <second@example.org>
To: Alice <alice@example.org>
From: Bob2 <bob@example.org>
Chat-Version: 1.0

Message content

-- 
Second signature";

        receive_imf(&alice, first_message, false).await?;
        let contact = Contact::load_from_db(&alice, bob_contact_id).await?;
        assert_eq!(contact.get_status(), "First signature");
        assert_eq!(contact.get_display_name(), "Bob1");

        receive_imf(&alice, second_message, false).await?;
        let contact = Contact::load_from_db(&alice, bob_contact_id).await?;
        assert_eq!(contact.get_status(), "Second signature");
        assert_eq!(contact.get_display_name(), "Bob2");

        // Duplicate message, should be ignored
        receive_imf(&alice, first_message, false).await?;

        // No change because last message is duplicate of the first.
        let contact = Contact::load_from_db(&alice, bob_contact_id).await?;
        assert_eq!(contact.get_status(), "Second signature");
        assert_eq!(contact.get_display_name(), "Bob2");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ignore_footer_status_from_mailinglist() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;
        let bob_id = Contact::add_or_lookup(&t, "", "bob@example.net", Origin::IncomingUnknownCc)
            .await?
            .0;
        let bob = Contact::load_from_db(&t, bob_id).await?;
        assert_eq!(bob.get_status(), "");
        assert_eq!(Chatlist::try_load(&t, 0, None, None).await?.len(), 0);

        receive_imf(
            &t,
            b"From: Bob <bob@example.net>
To: Alice <alice@example.org>
Message-ID: <1@example.org>
Subject: first message

body 1

--
Original signature",
            false,
        )
        .await?;
        let one2one_chat_id = t.get_last_msg().await.chat_id;
        let bob = Contact::load_from_db(&t, bob_id).await?;
        assert_eq!(bob.get_status(), "Original signature");

        receive_imf(
            &t,
            b"From: Bob <bob@example.net>
Sender: ml@example.net
To: Alice <alice@example.org>
Message-ID: <2@example.net>
Precedence: list
Subject: second message

body 2

--
The modified signature
--
Tap here to unsubscribe ...",
            false,
        )
        .await?;
        let ml_chat_id = t.get_last_msg().await.chat_id;
        let bob = Contact::load_from_db(&t, bob_id).await?;
        assert_eq!(bob.get_status(), "Original signature");

        receive_imf(
            &t,
            b"From: Bob <bob@example.net>
To: Alice <alice@example.org>
Message-ID: <3@example.org>
Subject: third message

body 3

--
Original signature updated",
            false,
        )
        .await?;
        let bob = Contact::load_from_db(&t, bob_id).await?;
        assert_eq!(bob.get_status(), "Original signature updated");
        assert_eq!(get_chat_msgs(&t, one2one_chat_id, 0).await?.len(), 2);
        assert_eq!(get_chat_msgs(&t, ml_chat_id, 0).await?.len(), 1);
        assert_eq!(Chatlist::try_load(&t, 0, None, None).await?.len(), 2);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_assignment_private_classical_reply() {
        for outgoing_is_classical in &[true, false] {
            let t = TestContext::new_alice().await;
            t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

            receive_imf(
                &t,
                format!(
                    r#"Received: from mout.gmx.net (mout.gmx.net [212.227.17.22])
Subject: =?utf-8?q?single_reply-to?=
{}
Date: Fri, 28 May 2021 10:15:05 +0000
To: Bob <bob@example.com>, <claire@example.com>
From: Alice <alice@example.org>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no
Content-Transfer-Encoding: quoted-printable

Hello, I've just created the group "single reply-to" for us."#,
                    if *outgoing_is_classical {
                        r"Message-ID: abcd@gmx.de"
                    } else {
                        r"Chat-Group-ID: eJ_llQIXf0K
Chat-Group-Name: =?utf-8?q?single_reply-to?=
References: <Gr.eJ_llQIXf0K.buxmrnMmG0Y@gmx.de>
Chat-Version: 1.0
Message-ID: <Gr.eJ_llQIXf0K.buxmrnMmG0Y@gmx.de>"
                    }
                )
                .as_bytes(),
                false,
            )
            .await
            .unwrap();

            let group_msg = t.get_last_msg().await;
            assert_eq!(
                group_msg.text.unwrap(),
                if *outgoing_is_classical {
                    "single reply-to – Hello, I\'ve just created the group \"single reply-to\" for us."
                } else {
                    "Hello, I've just created the group \"single reply-to\" for us."
                }
            );
            let group_chat = Chat::load_from_db(&t, group_msg.chat_id).await.unwrap();
            assert_eq!(group_chat.typ, Chattype::Group);
            assert_eq!(group_chat.name, "single reply-to");

            receive_imf(
                &t,
                format!(
                    r#"Subject: Re: single reply-to
To: "Alice" <alice@example.org>
References: <{0}>
 <{0}>
From: Bob <bob@example.com>
Message-ID: <028674eb-77f9-4ad1-1c30-e93e18b891c8@testrun.org>
Date: Fri, 28 May 2021 12:17:03 +0200
User-Agent: Mozilla/5.0 (X11; Linux x86_64; rv:78.0) Gecko/20100101
 Thunderbird/78.10.2
MIME-Version: 1.0
In-Reply-To: <{0}>

Private reply"#,
                    if *outgoing_is_classical {
                        "abcd@gmx.de"
                    } else {
                        "Gr.eJ_llQIXf0K.buxmrnMmG0Y@gmx.de"
                    }
                )
                .as_bytes(),
                false,
            )
            .await
            .unwrap();

            let private_msg = t.get_last_msg().await;
            assert_eq!(private_msg.text.unwrap(), "Private reply");
            let private_chat = Chat::load_from_db(&t, private_msg.chat_id).await.unwrap();
            assert_eq!(private_chat.typ, Chattype::Single);
            assert_ne!(private_msg.chat_id, group_msg.chat_id);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_assignment_private_chat_reply() {
        for (outgoing_is_classical, outgoing_has_multiple_recipients) in
            &[(true, true), (false, true), (false, false)]
        {
            let t = TestContext::new_alice().await;
            t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

            receive_imf(
                &t,
                format!(
                    r#"Received: from mout.gmx.net (mout.gmx.net [212.227.17.22])
Subject: =?utf-8?q?single_reply-to?=
{}
Date: Fri, 28 May 2021 10:15:05 +0000
To: Bob <bob@example.com>{}
From: Alice <alice@example.org>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no
Content-Transfer-Encoding: quoted-printable

Hello, I've just created the group "single reply-to" for us."#,
                    if *outgoing_is_classical {
                        r"Message-ID: abcd@gmx.de"
                    } else {
                        r"Chat-Group-ID: eJ_llQIXf0K
Chat-Group-Name: =?utf-8?q?single_reply-to?=
References: <Gr.iy1KCE2y65_.mH2TM52miv9@testrun.org>
Chat-Version: 1.0
Message-ID: <Gr.iy1KCE2y65_.mH2TM52miv9@testrun.org>"
                    },
                    if *outgoing_has_multiple_recipients {
                        ", <claire@example.com>"
                    } else {
                        ""
                    }
                )
                .as_bytes(),
                false,
            )
            .await
            .unwrap();
            let group_msg = t.get_last_msg().await;
            assert_eq!(
                group_msg.text.unwrap(),
                if *outgoing_is_classical {
                    "single reply-to – Hello, I\'ve just created the group \"single reply-to\" for us."
                } else {
                    "Hello, I've just created the group \"single reply-to\" for us."
                }
            );
            let group_chat = Chat::load_from_db(&t, group_msg.chat_id).await.unwrap();
            assert_eq!(group_chat.typ, Chattype::Group);
            assert_eq!(group_chat.name, "single reply-to");

            receive_imf(
                &t,
                format!(
                    r#"Subject: =?utf-8?q?Re=3A_single_reply-to?=
MIME-Version: 1.0
In-Reply-To: <{0}>
Date: Sat, 03 Jul 2021 20:00:26 +0000
Chat-Version: 1.0
Message-ID: <Mr.CJFwF5hwn8W.Pd-GGH5m32k@gmx.de>
To: <alice@example.org>
From: <bob@example.com>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no
Content-Transfer-Encoding: quoted-printable

> Hello, I've just created the group "single reply-to" for us.

Private reply

=2D-
Sent with my Delta Chat Messenger: https://delta.chat

"#,
                    if *outgoing_is_classical {
                        "abcd@gmx.de"
                    } else {
                        "Gr.iy1KCE2y65_.mH2TM52miv9@testrun.org"
                    }
                )
                .as_bytes(),
                false,
            )
            .await
            .unwrap();

            let private_msg = t.get_last_msg().await;
            assert_eq!(private_msg.text.unwrap(), "Private reply");
            let private_chat = Chat::load_from_db(&t, private_msg.chat_id).await.unwrap();
            assert_eq!(private_chat.typ, Chattype::Single);
            assert_ne!(private_msg.chat_id, group_msg.chat_id);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_assignment_nonprivate_classical_reply() {
        for outgoing_is_classical in &[true, false] {
            let t = TestContext::new_alice().await;
            t.set_config(Config::ShowEmails, Some("2")).await.unwrap();

            receive_imf(
                &t,
                format!(
                    r#"Received: from mout.gmx.net (mout.gmx.net [212.227.17.22])
Subject: =?utf-8?q?single_reply-to?=
{}
To: Bob <bob@example.com>, <claire@example.com>
From: Alice <alice@example.org>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no
Content-Transfer-Encoding: quoted-printable

Hello, I've just created the group "single reply-to" for us."#,
                    if *outgoing_is_classical {
                        r"Message-ID: abcd@gmx.de"
                    } else {
                        r"Chat-Group-ID: eJ_llQIXf0K
Chat-Group-Name: =?utf-8?q?single_reply-to?=
References: <Gr.eJ_llQIXf0K.buxmrnMmG0Y@gmx.de>
Chat-Version: 1.0
Message-ID: <Gr.eJ_llQIXf0K.buxmrnMmG0Y@gmx.de>"
                    }
                )
                .as_bytes(),
                false,
            )
            .await
            .unwrap();

            let group_msg = t.get_last_msg().await;
            assert_eq!(
                group_msg.text.unwrap(),
                if *outgoing_is_classical {
                    "single reply-to – Hello, I\'ve just created the group \"single reply-to\" for us."
                } else {
                    "Hello, I've just created the group \"single reply-to\" for us."
                }
            );
            let group_chat = Chat::load_from_db(&t, group_msg.chat_id).await.unwrap();
            assert_eq!(group_chat.typ, Chattype::Group);
            assert_eq!(group_chat.name, "single reply-to");

            // =============== Receive another outgoing message and check that it is put into the same chat ===============
            receive_imf(
                &t,
                format!(
                    r#"Received: from mout.gmx.net (mout.gmx.net [212.227.17.22])
Subject: Out subj
To: "Bob" <bob@example.com>, "Claire" <claire@example.com>
From: Alice <alice@example.org>
Message-ID: <outgoing@testrun.org>
MIME-Version: 1.0
In-Reply-To: <{0}>

Outgoing reply to all"#,
                    if *outgoing_is_classical {
                        "abcd@gmx.de"
                    } else {
                        "Gr.eJ_llQIXf0K.buxmrnMmG0Y@gmx.de"
                    }
                )
                .as_bytes(),
                false,
            )
            .await
            .unwrap();

            let reply = t.get_last_msg().await;
            assert_eq!(reply.text.unwrap(), "Out subj – Outgoing reply to all");
            let reply_chat = Chat::load_from_db(&t, reply.chat_id).await.unwrap();
            assert_eq!(reply_chat.typ, Chattype::Group);
            assert_eq!(reply.chat_id, group_msg.chat_id);

            // =============== Receive an incoming message and check that it is put into the same chat ===============
            receive_imf(
                &t,
                br#"Received: from mout.gmx.net (mout.gmx.net [212.227.17.22])
Subject: In subj
To: "Bob" <bob@example.com>, "Claire" <claire@example.com>
From: alice <alice@example.org>
Message-ID: <xyz@testrun.org>
MIME-Version: 1.0
In-Reply-To: <outgoing@testrun.org>

Reply to all"#,
                false,
            )
            .await
            .unwrap();

            let reply = t.get_last_msg().await;
            assert_eq!(reply.text.unwrap(), "In subj – Reply to all");
            let reply_chat = Chat::load_from_db(&t, reply.chat_id).await.unwrap();
            assert_eq!(reply_chat.typ, Chattype::Group);
            assert_eq!(reply.chat_id, group_msg.chat_id);
        }
    }

    /// Tests that replies to similar ad hoc groups are correctly assigned to chats.
    ///
    /// The difficutly here is that ad hoc groups don't have unique group IDs, because both
    /// messages have the same recipient lists and only differ in the subject and message contents.
    /// The messages can be properly assigned to chats only using the In-Reply-To or References
    /// headers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chat_assignment_adhoc() -> Result<()> {
        let alice = TestContext::new_alice().await;
        let bob = TestContext::new_alice().await;
        alice.set_config(Config::ShowEmails, Some("2")).await?;
        bob.set_config(Config::ShowEmails, Some("2")).await?;

        let first_thread_mime = br#"Subject: First thread
Message-ID: first@example.org
To: Alice <alice@example.org>, Bob <bob@example.net>
From: Claire <claire@example.org>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

First thread."#;
        let second_thread_mime = br#"Subject: Second thread
Message-ID: second@example.org
To: Alice <alice@example.org>, Bob <bob@example.net>
From: Claire <claire@example.org>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

Second thread."#;

        // Alice receives two classic emails from Claire.
        receive_imf(&alice, first_thread_mime, false).await?;
        let alice_first_msg = alice.get_last_msg().await;
        receive_imf(&alice, second_thread_mime, false).await?;
        let alice_second_msg = alice.get_last_msg().await;

        // Bob receives the same two emails.
        receive_imf(&bob, first_thread_mime, false).await?;
        let bob_first_msg = bob.get_last_msg().await;
        receive_imf(&bob, second_thread_mime, false).await?;
        let bob_second_msg = bob.get_last_msg().await;

        // Messages go to separate chats both for Alice and Bob.
        assert!(alice_first_msg.chat_id != alice_second_msg.chat_id);
        assert!(bob_first_msg.chat_id != bob_second_msg.chat_id);

        // Alice replies to both chats. Bob receives two messages and assigns them to corresponding
        // chats.
        alice_first_msg.chat_id.accept(&alice).await?;
        let alice_first_reply = alice
            .send_text(alice_first_msg.chat_id, "First reply")
            .await;
        let bob_first_reply = bob.recv_msg(&alice_first_reply).await;
        assert_eq!(bob_first_reply.chat_id, bob_first_msg.chat_id);

        alice_second_msg.chat_id.accept(&alice).await?;
        let alice_second_reply = alice
            .send_text(alice_second_msg.chat_id, "Second reply")
            .await;
        let bob_second_reply = bob.recv_msg(&alice_second_reply).await;
        assert_eq!(bob_second_reply.chat_id, bob_second_msg.chat_id);

        // Alice adds Fiona to both ad hoc groups.
        let fiona = TestContext::new_fiona().await;
        let (alice_fiona_contact_id, _) = Contact::add_or_lookup(
            &alice,
            "Fiona",
            "fiona@example.net",
            Origin::IncomingUnknownTo,
        )
        .await?;

        chat::add_contact_to_chat(&alice, alice_first_msg.chat_id, alice_fiona_contact_id).await?;
        let alice_first_invite = alice.pop_sent_msg().await;
        let fiona_first_invite = fiona.recv_msg(&alice_first_invite).await;

        chat::add_contact_to_chat(&alice, alice_second_msg.chat_id, alice_fiona_contact_id).await?;
        let alice_second_invite = alice.pop_sent_msg().await;
        let fiona_second_invite = fiona.recv_msg(&alice_second_invite).await;

        // Fiona was added to two separate chats and should see two separate chats, even though they
        // don't have different group IDs to distinguish them.
        assert!(fiona_first_invite.chat_id != fiona_second_invite.chat_id);

        Ok(())
    }

    /// Test that read receipts don't create chats.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_read_receipts_dont_create_chats() -> Result<()> {
        let alice = TestContext::new_alice().await;
        let bob = TestContext::new_bob().await;
        let alice_chat = alice.create_chat(&bob).await;

        // Alice sends a message to Bob.
        assert_eq!(Chatlist::try_load(&bob, 0, None, None).await?.len(), 0);
        bob.recv_msg(&alice.send_text(alice_chat.id, "Message").await)
            .await;
        let received_msg = bob.get_last_msg().await;

        // Alice deletes the chat.
        alice_chat.id.delete(&alice).await?;
        let chats = Chatlist::try_load(&alice, 0, None, None).await?;
        assert_eq!(chats.len(), 0);

        // Bob sends a read receipt.
        let mdn_mimefactory =
            crate::mimefactory::MimeFactory::from_mdn(&bob, &received_msg, vec![]).await?;
        let rendered_mdn = mdn_mimefactory.render(&bob).await?;
        let mdn_body = rendered_mdn.message;

        // Alice receives the read receipt.
        receive_imf(&alice, mdn_body.as_bytes(), false).await?;

        // Chat should not pop up in the chatlist.
        let chats = Chatlist::try_load(&alice, 0, None, None).await?;
        assert_eq!(chats.len(), 0);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gmx_forwarded_msg() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;

        receive_imf(
            &t,
            include_bytes!("../test-data/message/gmx-forward.eml"),
            false,
        )
        .await?;

        let msg = t.get_last_msg().await;
        assert!(msg.has_html());
        assert_eq!(msg.id.get_html(&t).await?.unwrap().replace("\r\n", "\n"), "<html><head></head><body><div style=\"font-family: Verdana;font-size: 12.0px;\"><div>&nbsp;</div>\n\n<div>&nbsp;\n<div>&nbsp;\n<div data-darkreader-inline-border-left=\"\" name=\"quote\" style=\"margin: 10px 5px 5px 10px; padding: 10px 0px 10px 10px; border-left: 2px solid rgb(195, 217, 229); overflow-wrap: break-word; --darkreader-inline-border-left:#274759;\">\n<div style=\"margin:0 0 10px 0;\"><b>Gesendet:</b>&nbsp;Donnerstag, 12. August 2021 um 15:52 Uhr<br/>\n<b>Von:</b>&nbsp;&quot;Claire&quot; &lt;claire@example.org&gt;<br/>\n<b>An:</b>&nbsp;alice@example.org<br/>\n<b>Betreff:</b>&nbsp;subject</div>\n\n<div name=\"quoted-content\">bodytext</div>\n</div>\n</div>\n</div></div></body></html>\n\n");

        Ok(())
    }

    /// Tests that user is notified about new incoming contact requests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_incoming_contact_request() -> Result<()> {
        let t = TestContext::new_alice().await;

        receive_imf(&t, MSGRMSG, false).await?;
        let msg = t.get_last_msg().await;
        let chat = chat::Chat::load_from_db(&t, msg.chat_id).await?;
        assert!(chat.is_contact_request());

        loop {
            let event = t
                .evtracker
                .get_matching(|evt| matches!(evt, EventType::IncomingMsg { .. }))
                .await;
            match event {
                EventType::IncomingMsg { chat_id, msg_id } => {
                    assert_eq!(msg.chat_id, chat_id);
                    assert_eq!(msg.id, msg_id);
                    return Ok(());
                }
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_get_parent_message() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;

        let mime = br#"Subject: First
Message-ID: first@example.net
To: Alice <alice@example.org>
From: Bob <bob@example.net>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

First."#;
        receive_imf(&t, mime, false).await?;
        let first = t.get_last_msg().await;
        let mime = br#"Subject: Second
Message-ID: second@example.net
To: Alice <alice@example.org>
From: Bob <bob@example.net>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

First."#;
        receive_imf(&t, mime, false).await?;
        let second = t.get_last_msg().await;
        let mime = br#"Subject: Third
Message-ID: third@example.net
To: Alice <alice@example.org>
From: Bob <bob@example.net>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

First."#;
        receive_imf(&t, mime, false).await?;
        let third = t.get_last_msg().await;

        let mime = br#"Subject: Message with references.
Message-ID: second@example.net
To: Alice <alice@example.org>
From: Bob <bob@example.net>
In-Reply-To: <third@example.net>
References: <second@example.net> <nonexistent@example.net> <first@example.net>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

Message with references."#;
        let mime_parser = MimeMessage::from_bytes(&t, &mime[..]).await?;

        let parent = get_parent_message(&t, &mime_parser).await?.unwrap();
        assert_eq!(parent.id, first.id);

        message::delete_msgs(&t, &[first.id]).await?;
        let parent = get_parent_message(&t, &mime_parser).await?.unwrap();
        assert_eq!(parent.id, second.id);

        message::delete_msgs(&t, &[second.id]).await?;
        let parent = get_parent_message(&t, &mime_parser).await?.unwrap();
        assert_eq!(parent.id, third.id);

        message::delete_msgs(&t, &[third.id]).await?;
        let parent = get_parent_message(&t, &mime_parser).await?;
        assert!(parent.is_none());

        Ok(())
    }

    /// Test a message with RFC 1847 encapsulation as created by Thunderbird.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_rfc1847_encapsulation() -> Result<()> {
        let alice = TestContext::new_alice().await;
        let bob = TestContext::new_bob().await;
        alice.configure_addr("alice@example.org").await;

        // Alice sends an Autocrypt message to Bob so Bob gets Alice's key.
        let chat_alice = alice.create_chat(&bob).await;
        let first_msg = alice
            .send_text(chat_alice.id, "Sending Alice key to Bob.")
            .await;
        bob.recv_msg(&first_msg).await;
        message::delete_msgs(&bob, &[bob.get_last_msg().await.id]).await?;

        bob.set_config(Config::ShowEmails, Some("2")).await?;

        // Alice sends a message to Bob using Thunderbird.
        let raw = include_bytes!("../test-data/message/rfc1847_encapsulation.eml");
        receive_imf(&bob, raw, false).await?;

        let msg = bob.get_last_msg().await;
        assert!(msg.get_showpadlock());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_invalid_to_address() -> Result<()> {
        let alice = TestContext::new_alice().await;

        let mime = include_bytes!("../test-data/message/invalid_email_to.eml");

        // receive_imf should not fail on this mail with invalid To: field
        receive_imf(&alice, mime, false).await?;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_reply_from_different_addr() -> Result<()> {
        let t = TestContext::new_alice().await;
        t.set_config(Config::ShowEmails, Some("2")).await?;

        // Alice creates a 2-person-group with Bob
        receive_imf(
            &t,
            br#"Subject: =?utf-8?q?Januar_13-19?=
Chat-Group-ID: qetqsutor7a
Chat-Group-Name: =?utf-8?q?Januar_13-19?=
MIME-Version: 1.0
References: <Gr.qetqsutor7a.Aresxresy-4@deltachat.de>
Date: Mon, 20 Dec 2021 12:15:01 +0000
Chat-Version: 1.0
Message-ID: <Gr.qetqsutor7a.Aresxresy-4@deltachat.de>
To: <bob@example.org>
From: <alice@example.org>
Content-Type: text/plain; charset=utf-8; format=flowed; delsp=no

Hi, I created a group"#,
            false,
        )
        .await?;
        let msg_out = t.get_last_msg().await;
        assert_eq!(msg_out.from_id, ContactId::SELF);
        assert_eq!(msg_out.text.unwrap(), "Hi, I created a group");
        assert_eq!(msg_out.in_reply_to, None);

        // Bob replies from a different address
        receive_imf(
            &t,
            b"Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: quoted-printable
From: <bob-alias@example.com>
Mime-Version: 1.0 (1.0)
Subject: Re: Januar 13-19
Date: Mon, 20 Dec 2021 13:54:55 +0100
Message-Id: <ERTSYSX-ERYSASQZS@example.com>
References: <Gr.qetqsutor7a.Aresxresy-4@deltachat.de>
In-Reply-To: <Gr.qetqsutor7a.Aresxresy-4@deltachat.de>
To: holger <alice@example.org>

Reply from different address
",
            false,
        )
        .await?;
        let msg_in = t.get_last_msg().await;
        assert_eq!(msg_in.to_id, ContactId::SELF);
        assert_eq!(msg_in.text.unwrap(), "Reply from different address");
        assert_eq!(
            msg_in.in_reply_to.unwrap(),
            "Gr.qetqsutor7a.Aresxresy-4@deltachat.de"
        );
        assert_eq!(
            msg_in.param.get(Param::OverrideSenderDisplayname),
            Some("bob-alias@example.com")
        );

        assert_eq!(msg_in.chat_id, msg_out.chat_id);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_long_filenames() -> Result<()> {
        let mut tcm = TestContextManager::new().await;
        let alice = tcm.alice().await;
        let bob = tcm.bob().await;

        for filename_sent in &[
            "foo.bar very long file name test baz.tar.gz",
            "foobarabababababababbababababverylongfilenametestbaz.tar.gz",
            "fooo...tar.gz",
            "foo. .tar.gz",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tar.gz",
            "a.tar.gz",
            "a.a..a.a.a.a.tar.gz",
        ] {
            let attachment = alice.blobdir.join(filename_sent);
            let content = format!("File content of {}", filename_sent);
            tokio::fs::write(&attachment, content.as_bytes()).await?;

            let mut msg_alice = Message::new(Viewtype::File);
            msg_alice.set_file(attachment.to_str().unwrap(), None);
            let alice_chat = alice.create_chat(&bob).await;
            let sent = alice.send_msg(alice_chat.id, &mut msg_alice).await;
            println!("{}", sent.payload());

            let msg_bob = bob.recv_msg(&sent).await;

            async fn check_message(msg: &Message, t: &TestContext, content: &str) {
                assert_eq!(msg.get_viewtype(), Viewtype::File);
                let resulting_filename = msg.get_filename().unwrap();
                let path = msg.get_file(t).unwrap();
                assert!(
                    resulting_filename.ends_with(".tar.gz"),
                    "{:?} doesn't end with .tar.gz, path: {:?}",
                    resulting_filename,
                    path
                );
                assert!(
                    path.to_str().unwrap().ends_with(".tar.gz"),
                    "path {:?} doesn't end with .tar.gz",
                    path
                );
                assert_eq!(fs::read_to_string(path).await.unwrap(), content);
            }
            check_message(&msg_alice, &alice, &content).await;
            check_message(&msg_bob, &bob, &content).await;
        }

        Ok(())
    }

    /// Tests that contact request is accepted automatically on outgoing message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_accept_outgoing() -> Result<()> {
        let mut tcm = TestContextManager::new().await;
        let alice1 = tcm.alice().await;
        let alice2 = tcm.alice().await;
        let bob1 = tcm.bob().await;
        let bob2 = tcm.bob().await;

        let bob1_chat = bob1.create_chat(&alice1).await;
        let sent = bob1.send_text(bob1_chat.id, "Hello!").await;

        alice1.recv_msg(&sent).await;
        alice2.recv_msg(&sent).await;
        let alice1_msg = bob2.recv_msg(&sent).await;
        assert_eq!(alice1_msg.text.unwrap(), "Hello!");
        let alice1_chat = chat::Chat::load_from_db(&alice1, alice1_msg.chat_id).await?;
        assert!(alice1_chat.is_contact_request());

        let alice2_msg = alice2.get_last_msg().await;
        assert_eq!(alice2_msg.text.unwrap(), "Hello!");
        let alice2_chat = chat::Chat::load_from_db(&alice2, alice2_msg.chat_id).await?;
        assert!(alice2_chat.is_contact_request());

        let bob1_msg = bob1.get_last_msg().await;
        assert_eq!(bob1_msg.text.unwrap(), "Hello!");
        let bob1_chat = chat::Chat::load_from_db(&bob1, bob1_msg.chat_id).await?;
        assert!(!bob1_chat.is_contact_request());

        let bob2_msg = bob2.get_last_msg().await;
        assert_eq!(bob2_msg.text.unwrap(), "Hello!");
        let bob2_chat = chat::Chat::load_from_db(&bob2, bob2_msg.chat_id).await?;
        assert!(!bob2_chat.is_contact_request());

        // Alice sends reply.
        alice1_msg.chat_id.accept(&alice1).await.unwrap();
        let sent = alice1.send_text(alice1_chat.id, "Hi!").await;
        alice2.recv_msg(&sent).await;

        // Second device automatically accepts the contact request.
        let alice2_chat = chat::Chat::load_from_db(&alice2, alice2_msg.chat_id).await?;
        assert!(!alice2_chat.is_contact_request());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_outgoing_private_reply_multidevice() -> Result<()> {
        let mut tcm = TestContextManager::new().await;
        let alice1 = tcm.alice().await;
        let alice2 = tcm.alice().await;
        let bob = tcm.bob().await;

        // =============== Bob creates a group ===============
        let group_id =
            chat::create_group_chat(&bob, ProtectionStatus::Unprotected, "Group").await?;
        chat::add_to_chat_contacts_table(
            &bob,
            group_id,
            bob.add_or_lookup_contact(&alice1).await.id,
        )
        .await?;
        chat::add_to_chat_contacts_table(
            &bob,
            group_id,
            Contact::create(&bob, "", "charlie@example.org").await?,
        )
        .await?;

        // =============== Bob sends the first message to the group ===============
        let sent = bob.send_text(group_id, "Hello all!").await;
        alice1.recv_msg(&sent).await;
        alice2.recv_msg(&sent).await;

        // =============== Alice answers privately with device 1 ===============
        let received = alice1.get_last_msg().await;
        let alice1_bob_contact = alice1.add_or_lookup_contact(&bob).await;
        assert_eq!(received.from_id, alice1_bob_contact.id);
        assert_eq!(received.to_id, ContactId::SELF);
        assert!(!received.hidden);
        assert_eq!(received.text, Some("Hello all!".to_string()));
        assert_eq!(received.in_reply_to, None);
        assert_eq!(received.chat_blocked, Blocked::Request);

        let received_group = Chat::load_from_db(&alice1, received.chat_id).await?;
        assert_eq!(received_group.typ, Chattype::Group);
        assert_eq!(received_group.name, "Group");
        assert_eq!(received_group.can_send(&alice1).await?, false); // Can't send because it's Blocked::Request

        let mut msg_out = Message::new(Viewtype::Text);
        msg_out.set_text(Some("Private reply".to_string()));

        assert_eq!(received_group.blocked, Blocked::Request);
        msg_out.set_quote(&alice1, Some(&received)).await?;
        let alice1_bob_chat = alice1.create_chat(&bob).await;
        let sent2 = alice1.send_msg(alice1_bob_chat.id, &mut msg_out).await;
        alice2.recv_msg(&sent2).await;

        // =============== Alice's second device receives the message ===============
        let received = alice2.get_last_msg().await;

        // That's a regression test for https://github.com/deltachat/deltachat-core-rust/issues/2949:
        assert_eq!(received.chat_id, alice2.get_chat(&bob).await.unwrap().id);

        let alice2_bob_contact = alice2.add_or_lookup_contact(&bob).await;
        assert_eq!(received.from_id, ContactId::SELF);
        assert_eq!(received.to_id, alice2_bob_contact.id);
        assert!(!received.hidden);
        assert_eq!(received.text, Some("Private reply".to_string()));
        assert_eq!(
            received.parent(&alice2).await?.unwrap().text,
            Some("Hello all!".to_string())
        );
        assert_eq!(received.chat_blocked, Blocked::Not);

        let received_chat = Chat::load_from_db(&alice2, received.chat_id).await?;
        assert_eq!(received_chat.typ, Chattype::Single);
        assert_eq!(received_chat.name, "bob@example.net");
        assert_eq!(received_chat.can_send(&alice2).await?, true);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_no_private_reply_to_blocked_account() -> Result<()> {
        let mut tcm = TestContextManager::new().await;
        let alice = tcm.alice().await;
        let bob = tcm.bob().await;

        // =============== Bob creates a group ===============
        let group_id =
            chat::create_group_chat(&bob, ProtectionStatus::Unprotected, "Group").await?;
        chat::add_to_chat_contacts_table(
            &bob,
            group_id,
            bob.add_or_lookup_contact(&alice).await.id,
        )
        .await?;

        // =============== Bob sends the first message to the group ===============
        let sent = bob.send_text(group_id, "Hello all!").await;
        alice.recv_msg(&sent).await;

        let chats = Chatlist::try_load(&bob, 0, None, None).await?;
        assert_eq!(chats.len(), 1);

        // =============== Bob blocks Alice ================
        Contact::block(&bob, bob.add_or_lookup_contact(&alice).await.id).await?;

        // =============== Alice replies private to Bob ==============
        let received = alice.get_last_msg().await;
        assert_eq!(received.text, Some("Hello all!".to_string()));

        let received_group = Chat::load_from_db(&alice, received.chat_id).await?;
        assert_eq!(received_group.typ, Chattype::Group);

        let mut msg_out = Message::new(Viewtype::Text);
        msg_out.set_text(Some("Private reply".to_string()));
        msg_out.set_quote(&alice, Some(&received)).await?;

        let alice_bob_chat = alice.create_chat(&bob).await;
        let sent2 = alice.send_msg(alice_bob_chat.id, &mut msg_out).await;
        bob.recv_msg(&sent2).await;

        // ========= check that no contact request was created ============
        let chats = Chatlist::try_load(&bob, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 1);
        let chat_id = chats.get_chat_id(0).unwrap();
        let chat = Chat::load_from_db(&bob, chat_id).await.unwrap();

        // since only chat is a group, no new open chat has been created
        assert_eq!(chat.typ, Chattype::Group);
        let received = bob.get_last_msg().await;
        assert_eq!(received.text, Some("Hello all!".to_string()));

        // =============== Bob unblocks Alice ================
        // test if the blocked chat is restored correctly
        Contact::unblock(&bob, bob.add_or_lookup_contact(&alice).await.id).await?;
        let chats = Chatlist::try_load(&bob, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 2);
        let chat_id = chats.get_chat_id(0).unwrap();
        let chat = Chat::load_from_db(&bob, chat_id).await.unwrap();
        assert_eq!(chat.typ, Chattype::Single);
        let received = bob.get_last_msg().await;
        assert_eq!(received.text, Some("Private reply".to_string()));

        Ok(())
    }
}
