//! Module to work with translatable stock strings.

use anyhow::{bail, Error};
use strum::EnumProperty;
use strum_macros::EnumProperty;

use crate::blob::BlobObject;
use crate::chat::{self, Chat, ChatId, ProtectionStatus};
use crate::config::Config;
use crate::contact::{Contact, ContactId, Origin};
use crate::context::Context;
use crate::message::{Message, Viewtype};
use crate::param::Param;
use crate::tools::timestamp_to_str;
use humansize::{file_size_opts, FileSize};

/// Stock strings
///
/// These identify the string to return in [Context.stock_str].  The
/// numbers must stay in sync with `deltachat.h` `DC_STR_*` constants.
///
/// See the `stock_*` methods on [Context] to use these.
///
/// [Context]: crate::context::Context
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive, EnumProperty)]
#[repr(u32)]
pub enum StockMessage {
    #[strum(props(fallback = "No messages."))]
    NoMessages = 1,

    #[strum(props(fallback = "Me"))]
    SelfMsg = 2,

    #[strum(props(fallback = "Draft"))]
    Draft = 3,

    #[strum(props(fallback = "Voice message"))]
    VoiceMessage = 7,

    #[strum(props(fallback = "Image"))]
    Image = 9,

    #[strum(props(fallback = "Video"))]
    Video = 10,

    #[strum(props(fallback = "Audio"))]
    Audio = 11,

    #[strum(props(fallback = "File"))]
    File = 12,

    #[strum(props(fallback = "GIF"))]
    Gif = 23,

    #[strum(props(fallback = "Encrypted message"))]
    EncryptedMsg = 24,

    #[strum(props(fallback = "End-to-end encryption available"))]
    E2eAvailable = 25,

    #[strum(props(fallback = "No encryption"))]
    EncrNone = 28,

    #[strum(props(fallback = "This message was encrypted for another setup."))]
    CantDecryptMsgBody = 29,

    #[strum(props(fallback = "Fingerprints"))]
    FingerPrints = 30,

    #[strum(props(fallback = "Return receipt"))]
    ReadRcpt = 31,

    #[strum(props(fallback = "This is a return receipt for the message \"%1$s\"."))]
    ReadRcptMailBody = 32,

    #[strum(props(fallback = "End-to-end encryption preferred"))]
    E2ePreferred = 34,

    #[strum(props(fallback = "%1$s verified."))]
    ContactVerified = 35,

    #[strum(props(fallback = "Cannot verify %1$s"))]
    ContactNotVerified = 36,

    #[strum(props(fallback = "Changed setup for %1$s"))]
    ContactSetupChanged = 37,

    #[strum(props(fallback = "Archived chats"))]
    ArchivedChats = 40,

    #[strum(props(fallback = "Autocrypt Setup Message"))]
    AcSetupMsgSubject = 42,

    #[strum(props(
        fallback = "This is the Autocrypt Setup Message used to transfer your key between clients.\n\nTo decrypt and use your key, open the message in an Autocrypt-compliant client and enter the setup code presented on the generating device."
    ))]
    AcSetupMsgBody = 43,

    #[strum(props(
        fallback = "Cannot login as \"%1$s\". Please check if the email address and the password are correct."
    ))]
    CannotLogin = 60,

    #[strum(props(fallback = "%1$s by %2$s."))]
    MsgActionByUser = 62,

    #[strum(props(fallback = "%1$s by me."))]
    MsgActionByMe = 63,

    #[strum(props(fallback = "Location streaming enabled."))]
    MsgLocationEnabled = 64,

    #[strum(props(fallback = "Location streaming disabled."))]
    MsgLocationDisabled = 65,

    #[strum(props(fallback = "Location"))]
    Location = 66,

    #[strum(props(fallback = "Sticker"))]
    Sticker = 67,

    #[strum(props(fallback = "Device messages"))]
    DeviceMessages = 68,

    #[strum(props(fallback = "Saved messages"))]
    SavedMessages = 69,

    #[strum(props(
        fallback = "Messages in this chat are generated locally by your Delta Chat app. \
                    Its makers use it to inform about app updates and problems during usage."
    ))]
    DeviceMessagesHint = 70,

    #[strum(props(fallback = "Welcome to Delta Chat! – \
                    Delta Chat looks and feels like other popular messenger apps, \
                    but does not involve centralized control, \
                    tracking or selling you, friends, colleagues or family out to large organizations.\n\n\
                    Technically, Delta Chat is an email application with a modern chat interface. \
                    Email in a new dress if you will 👻\n\n\
                    Use Delta Chat with anyone out of billions of people: just use their e-mail address. \
                    Recipients don't need to install Delta Chat, visit websites or sign up anywhere - \
                    however, of course, if they like, you may point them to 👉 https://get.delta.chat"))]
    WelcomeMessage = 71,

    #[strum(props(fallback = "Unknown sender for this chat. See 'info' for more details."))]
    UnknownSenderForChat = 72,

    #[strum(props(fallback = "Message from %1$s"))]
    SubjectForNewContact = 73,

    #[strum(props(fallback = "Failed to send message to %1$s."))]
    FailedSendingTo = 74,

    #[strum(props(fallback = "Video chat invitation"))]
    VideochatInvitation = 82,

    #[strum(props(fallback = "You are invited to a video chat, click %1$s to join."))]
    VideochatInviteMsgBody = 83,

    #[strum(props(fallback = "Error:\n\n“%1$s”"))]
    ConfigurationFailed = 84,

    #[strum(props(
        fallback = "⚠️ Date or time of your device seem to be inaccurate (%1$s).\n\n\
                    Adjust your clock ⏰🔧 to ensure your messages are received correctly."
    ))]
    BadTimeMsgBody = 85,

    #[strum(props(fallback = "⚠️ Your Delta Chat version might be outdated.\n\n\
                    This may cause problems because your chat partners use newer versions - \
                    and you are missing the latest features 😳\n\
                    Please check https://get.delta.chat or your app store for updates."))]
    UpdateReminderMsgBody = 86,

    #[strum(props(
        fallback = "Could not find your mail server.\n\nPlease check your internet connection."
    ))]
    ErrorNoNetwork = 87,

    // used in summaries, a noun, not a verb (not: "to reply")
    #[strum(props(fallback = "Reply"))]
    ReplyNoun = 90,

    #[strum(props(fallback = "You deleted the \"Saved messages\" chat.\n\n\
                    To use the \"Saved messages\" feature again, create a new chat with yourself."))]
    SelfDeletedMsgBody = 91,

    #[strum(props(
        fallback = "⚠️ The \"Delete messages from server\" feature now also deletes messages in folders other than Inbox, DeltaChat and Sent.\n\n\
                    ℹ️ To avoid accidentally deleting messages, we turned it off for you. Please turn it on again at \
                    Settings → \"Chats and Media\" → \"Delete messages from server\" to continue using it."
    ))]
    DeleteServerTurnedOff = 92,

    #[strum(props(fallback = "Forwarded"))]
    Forwarded = 97,

    #[strum(props(
        fallback = "⚠️ Your provider's storage is about to exceed, already %1$s%% are used.\n\n\
                    You may not be able to receive message when the storage is 100%% used.\n\n\
                    👉 Please check if you can delete old data in the provider's webinterface \
                    and consider to enable \"Settings / Delete Old Messages\". \
                    You can check your current storage usage anytime at \"Settings / Connectivity\"."
    ))]
    QuotaExceedingMsgBody = 98,

    #[strum(props(fallback = "%1$s message"))]
    PartialDownloadMsgBody = 99,

    #[strum(props(fallback = "Download maximum available until %1$s"))]
    DownloadAvailability = 100,

    #[strum(props(fallback = "Multi Device Synchronization"))]
    SyncMsgSubject = 101,

    #[strum(props(
        fallback = "This message is used to synchronize data between your devices.\n\n\
                    👉 If you see this message in Delta Chat, please update your Delta Chat apps on all devices."
    ))]
    SyncMsgBody = 102,

    #[strum(props(fallback = "Incoming Messages"))]
    IncomingMessages = 103,

    #[strum(props(fallback = "Outgoing Messages"))]
    OutgoingMessages = 104,

    #[strum(props(fallback = "Storage on %1$s"))]
    StorageOnDomain = 105,

    #[strum(props(fallback = "Connected"))]
    Connected = 107,

    #[strum(props(fallback = "Connecting…"))]
    Connecting = 108,

    #[strum(props(fallback = "Updating…"))]
    Updating = 109,

    #[strum(props(fallback = "Sending…"))]
    Sending = 110,

    #[strum(props(fallback = "Your last message was sent successfully."))]
    LastMsgSentSuccessfully = 111,

    #[strum(props(fallback = "Error: %1$s"))]
    Error = 112,

    #[strum(props(fallback = "Not supported by your provider."))]
    NotSupportedByProvider = 113,

    #[strum(props(fallback = "Messages"))]
    Messages = 114,

    #[strum(props(fallback = "Broadcast List"))]
    BroadcastList = 115,

    #[strum(props(fallback = "%1$s of %2$s used"))]
    PartOfTotallUsed = 116,

    #[strum(props(fallback = "%1$s invited you to join this group.\n\n\
                             Waiting for the device of %2$s to reply…"))]
    SecureJoinStarted = 117,

    #[strum(props(fallback = "%1$s replied, waiting for being added to the group…"))]
    SecureJoinReplies = 118,

    #[strum(props(fallback = "Scan to chat with %1$s"))]
    SetupContactQRDescription = 119,

    #[strum(props(fallback = "Scan to join group %1$s"))]
    SecureJoinGroupQRDescription = 120,

    #[strum(props(fallback = "Not connected"))]
    NotConnected = 121,

    #[strum(props(fallback = "%1$s changed their address from %2$s to %3$s"))]
    AeapAddrChanged = 122,

    #[strum(props(
        fallback = "You changed your email address from %1$s to %2$s.\n\nIf you now send a message to a verified group, contacts there will automatically replace the old with your new address.\n\nIt's highly advised to set up your old email provider to forward all emails to your new email address. Otherwise you might miss messages of contacts who did not get your new address yet."
    ))]
    AeapExplanationAndLink = 123,

    #[strum(props(fallback = "You changed group name from \"%1$s\" to \"%2$s\"."))]
    MsgYouChangedGrpName = 124,

    #[strum(props(fallback = "Group name changed from \"%1$s\" to \"%2$s\" by %3$s."))]
    MsgGrpNameChangedBy = 125,

    #[strum(props(fallback = "You changed the group image."))]
    MsgYouChangedGrpImg = 126,

    #[strum(props(fallback = "Group image changed by %1$s."))]
    MsgGrpImgChangedBy = 127,

    #[strum(props(fallback = "You added member %1$s."))]
    MsgYouAddMember = 128,

    #[strum(props(fallback = "Member %1$s added by %2$s."))]
    MsgAddMemberBy = 129,

    #[strum(props(fallback = "You removed member %1$s."))]
    MsgYouDelMember = 130,

    #[strum(props(fallback = "Member %1$s removed by %2$s."))]
    MsgDelMemberBy = 131,

    #[strum(props(fallback = "You left the group."))]
    MsgYouLeftGroup = 132,

    #[strum(props(fallback = "Group left by %1$s."))]
    MsgGroupLeftBy = 133,

    #[strum(props(fallback = "You deleted the group image."))]
    MsgYouDeletedGrpImg = 134,

    #[strum(props(fallback = "Group image deleted by %1$s."))]
    MsgGrpImgDeletedBy = 135,

    #[strum(props(fallback = "You enabled location streaming."))]
    MsgYouEnabledLocation = 136,

    #[strum(props(fallback = "Location streaming enabled by %1$s."))]
    MsgLocationEnabledBy = 137,

    #[strum(props(fallback = "You disabled message deletion timer."))]
    MsgYouDisabledEphemeralTimer = 138,

    #[strum(props(fallback = "Message deletion timer is disabled by %1$s."))]
    MsgEphemeralTimerDisabledBy = 139,

    // A fallback message for unknown timer values.
    // "s" stands for "second" SI unit here.
    #[strum(props(fallback = "You set message deletion timer to %1$s s."))]
    MsgYouEnabledEphemeralTimer = 140,

    #[strum(props(fallback = "Message deletion timer is set to %1$s s by %2$s."))]
    MsgEphemeralTimerEnabledBy = 141,

    #[strum(props(fallback = "You set message deletion timer to 1 minute."))]
    MsgYouEphemeralTimerMinute = 142,

    #[strum(props(fallback = "Message deletion timer is set to 1 minute by %1$s."))]
    MsgEphemeralTimerMinuteBy = 143,

    #[strum(props(fallback = "You set message deletion timer to 1 hour."))]
    MsgYouEphemeralTimerHour = 144,

    #[strum(props(fallback = "Message deletion timer is set to 1 hour by %1$s."))]
    MsgEphemeralTimerHourBy = 145,

    #[strum(props(fallback = "You set message deletion timer to 1 day."))]
    MsgYouEphemeralTimerDay = 146,

    #[strum(props(fallback = "Message deletion timer is set to 1 day by %1$s."))]
    MsgEphemeralTimerDayBy = 147,

    #[strum(props(fallback = "You set message deletion timer to 1 week."))]
    MsgYouEphemeralTimerWeek = 148,

    #[strum(props(fallback = "Message deletion timer is set to 1 week by %1$s."))]
    MsgEphemeralTimerWeekBy = 149,

    #[strum(props(fallback = "You set message deletion timer to %1$s minutes."))]
    MsgYouEphemeralTimerMinutes = 150,

    #[strum(props(fallback = "Message deletion timer is set to %1$s minutes by %2$s."))]
    MsgEphemeralTimerMinutesBy = 151,

    #[strum(props(fallback = "You set message deletion timer to %1$s hours."))]
    MsgYouEphemeralTimerHours = 152,

    #[strum(props(fallback = "Message deletion timer is set to %1$s hours by %2$s."))]
    MsgEphemeralTimerHoursBy = 153,

    #[strum(props(fallback = "You set message deletion timer to %1$s days."))]
    MsgYouEphemeralTimerDays = 154,

    #[strum(props(fallback = "Message deletion timer is set to %1$s days by %2$s."))]
    MsgEphemeralTimerDaysBy = 155,

    #[strum(props(fallback = "You set message deletion timer to %1$s weeks."))]
    MsgYouEphemeralTimerWeeks = 156,

    #[strum(props(fallback = "Message deletion timer is set to %1$s weeks by %2$s."))]
    MsgEphemeralTimerWeeksBy = 157,

    #[strum(props(fallback = "You enabled chat protection."))]
    YouEnabledProtection = 158,

    #[strum(props(fallback = "Chat protection enabled by %1$s."))]
    ProtectionEnabledBy = 159,

    #[strum(props(fallback = "You disabled chat protection."))]
    YouDisabledProtection = 160,

    #[strum(props(fallback = "Chat protection disabled by %1$s."))]
    ProtectionDisabledBy = 161,
}

impl StockMessage {
    /// Default untranslated strings for stock messages.
    ///
    /// These could be used in logging calls, so no logging here.
    fn fallback(self) -> &'static str {
        self.get_str("fallback").unwrap_or_default()
    }
}

async fn translated(context: &Context, id: StockMessage) -> String {
    context
        .translated_stockstrings
        .read()
        .await
        .get(&(id as usize))
        .map(AsRef::as_ref)
        .unwrap_or_else(|| id.fallback())
        .to_string()
}

/// Helper trait only meant to be implemented for [`String`].
trait StockStringMods: AsRef<str> + Sized {
    /// Substitutes the first replacement value if one is present.
    fn replace1(&self, replacement: impl AsRef<str>) -> String {
        self.as_ref()
            .replacen("%1$s", replacement.as_ref(), 1)
            .replacen("%1$d", replacement.as_ref(), 1)
            .replacen("%1$@", replacement.as_ref(), 1)
    }

    /// Substitutes the second replacement value if one is present.
    ///
    /// Be aware you probably should have also called [`StockStringMods::replace1`] if
    /// you are calling this.
    fn replace2(&self, replacement: impl AsRef<str>) -> String {
        self.as_ref()
            .replacen("%2$s", replacement.as_ref(), 1)
            .replacen("%2$d", replacement.as_ref(), 1)
            .replacen("%2$@", replacement.as_ref(), 1)
    }

    /// Substitutes the third replacement value if one is present.
    ///
    /// Be aware you probably should have also called [`StockStringMods::replace1`] and
    /// [`StockStringMods::replace2`] if you are calling this.
    fn replace3(&self, replacement: impl AsRef<str>) -> String {
        self.as_ref()
            .replacen("%3$s", replacement.as_ref(), 1)
            .replacen("%3$d", replacement.as_ref(), 1)
            .replacen("%3$@", replacement.as_ref(), 1)
    }
}

impl ContactId {
    /// Get contact name for stock string.
    async fn get_stock_name(self, context: &Context) -> String {
        Contact::get_by_id(context, self)
            .await
            .map(|contact| contact.get_name_n_addr())
            .unwrap_or_else(|_| self.to_string())
    }
}

impl StockStringMods for String {}

/// Stock string: `No messages.`.
pub(crate) async fn no_messages(context: &Context) -> String {
    translated(context, StockMessage::NoMessages).await
}

/// Stock string: `Me`.
pub(crate) async fn self_msg(context: &Context) -> String {
    translated(context, StockMessage::SelfMsg).await
}

/// Stock string: `Draft`.
pub(crate) async fn draft(context: &Context) -> String {
    translated(context, StockMessage::Draft).await
}

/// Stock string: `Voice message`.
pub(crate) async fn voice_message(context: &Context) -> String {
    translated(context, StockMessage::VoiceMessage).await
}

/// Stock string: `Image`.
pub(crate) async fn image(context: &Context) -> String {
    translated(context, StockMessage::Image).await
}

/// Stock string: `Video`.
pub(crate) async fn video(context: &Context) -> String {
    translated(context, StockMessage::Video).await
}

/// Stock string: `Audio`.
pub(crate) async fn audio(context: &Context) -> String {
    translated(context, StockMessage::Audio).await
}

/// Stock string: `File`.
pub(crate) async fn file(context: &Context) -> String {
    translated(context, StockMessage::File).await
}

/// Stock string: `Group name changed from "%1$s" to "%2$s".`.
pub(crate) async fn msg_grp_name(
    context: &Context,
    from_group: impl AsRef<str>,
    to_group: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouChangedGrpName)
            .await
            .replace1(from_group)
            .replace2(to_group)
    } else {
        translated(context, StockMessage::MsgGrpNameChangedBy)
            .await
            .replace1(from_group)
            .replace2(to_group)
            .replace3(by_contact.get_stock_name(context).await)
    }
}

pub(crate) async fn msg_grp_img_changed(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouChangedGrpImg).await
    } else {
        translated(context, StockMessage::MsgGrpImgChangedBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Member %1$s added.`.
///
/// The `added_member_addr` parameter should be an email address and is looked up in the
/// contacts to combine with the display name.
pub(crate) async fn msg_add_member(
    context: &Context,
    added_member_addr: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    let addr = added_member_addr.as_ref();
    let who = match Contact::lookup_id_by_addr(context, addr, Origin::Unknown).await {
        Ok(Some(contact_id)) => Contact::get_by_id(context, contact_id)
            .await
            .map(|contact| contact.get_name_n_addr())
            .unwrap_or_else(|_| addr.to_string()),
        _ => addr.to_string(),
    };
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouAddMember)
            .await
            .replace1(who)
    } else {
        translated(context, StockMessage::MsgAddMemberBy)
            .await
            .replace1(who)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Member %1$s removed.`.
///
/// The `removed_member_addr` parameter should be an email address and is looked up in
/// the contacts to combine with the display name.
pub(crate) async fn msg_del_member(
    context: &Context,
    removed_member_addr: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    let addr = removed_member_addr.as_ref();
    let who = match Contact::lookup_id_by_addr(context, addr, Origin::Unknown).await {
        Ok(Some(contact_id)) => Contact::get_by_id(context, contact_id)
            .await
            .map(|contact| contact.get_name_n_addr())
            .unwrap_or_else(|_| addr.to_string()),
        _ => addr.to_string(),
    };
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouDelMember)
            .await
            .replace1(who)
    } else {
        translated(context, StockMessage::MsgDelMemberBy)
            .await
            .replace1(who)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Group left.`.
pub(crate) async fn msg_group_left(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouLeftGroup).await
    } else {
        translated(context, StockMessage::MsgGroupLeftBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `GIF`.
pub(crate) async fn gif(context: &Context) -> String {
    translated(context, StockMessage::Gif).await
}

/// Stock string: `Encrypted message`.
pub(crate) async fn encrypted_msg(context: &Context) -> String {
    translated(context, StockMessage::EncryptedMsg).await
}

/// Stock string: `End-to-end encryption available.`.
pub(crate) async fn e2e_available(context: &Context) -> String {
    translated(context, StockMessage::E2eAvailable).await
}

/// Stock string: `No encryption.`.
pub(crate) async fn encr_none(context: &Context) -> String {
    translated(context, StockMessage::EncrNone).await
}

/// Stock string: `This message was encrypted for another setup.`.
pub(crate) async fn cant_decrypt_msg_body(context: &Context) -> String {
    translated(context, StockMessage::CantDecryptMsgBody).await
}

/// Stock string: `Fingerprints`.
pub(crate) async fn finger_prints(context: &Context) -> String {
    translated(context, StockMessage::FingerPrints).await
}

/// Stock string: `Return receipt`.
pub(crate) async fn read_rcpt(context: &Context) -> String {
    translated(context, StockMessage::ReadRcpt).await
}

/// Stock string: `This is a return receipt for the message "%1$s".`.
pub(crate) async fn read_rcpt_mail_body(context: &Context, message: impl AsRef<str>) -> String {
    translated(context, StockMessage::ReadRcptMailBody)
        .await
        .replace1(message)
}

/// Stock string: `Group image deleted.`.
pub(crate) async fn msg_grp_img_deleted(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouDeletedGrpImg).await
    } else {
        translated(context, StockMessage::MsgGrpImgDeletedBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `End-to-end encryption preferred.`.
pub(crate) async fn e2e_preferred(context: &Context) -> String {
    translated(context, StockMessage::E2ePreferred).await
}

/// Stock string: `%1$s invited you to join this group. Waiting for the device of %2$s to reply…`.
pub(crate) async fn secure_join_started(
    context: &Context,
    inviter_contact_id: ContactId,
) -> String {
    if let Ok(contact) = Contact::get_by_id(context, inviter_contact_id).await {
        translated(context, StockMessage::SecureJoinStarted)
            .await
            .replace1(contact.get_name_n_addr())
            .replace2(contact.get_display_name())
    } else {
        format!(
            "secure_join_started: unknown contact {}",
            inviter_contact_id
        )
    }
}

/// Stock string: `%1$s replied, waiting for being added to the group…`.
pub(crate) async fn secure_join_replies(context: &Context, contact_id: ContactId) -> String {
    if let Ok(contact) = Contact::get_by_id(context, contact_id).await {
        translated(context, StockMessage::SecureJoinReplies)
            .await
            .replace1(contact.get_display_name())
    } else {
        format!("secure_join_replies: unknown contact {}", contact_id)
    }
}

/// Stock string: `Scan to chat with %1$s`.
pub(crate) async fn setup_contact_qr_description(
    context: &Context,
    display_name: &str,
    addr: &str,
) -> String {
    let name = if display_name == addr {
        addr.to_owned()
    } else {
        format!("{} ({})", display_name, addr)
    };
    translated(context, StockMessage::SetupContactQRDescription)
        .await
        .replace1(name)
}

/// Stock string: `Scan to join %1$s`.
pub(crate) async fn secure_join_group_qr_description(context: &Context, chat: &Chat) -> String {
    translated(context, StockMessage::SecureJoinGroupQRDescription)
        .await
        .replace1(chat.get_name())
}

/// Stock string: `%1$s verified.`.
pub(crate) async fn contact_verified(context: &Context, contact: &Contact) -> String {
    let addr = contact.get_name_n_addr();
    translated(context, StockMessage::ContactVerified)
        .await
        .replace1(addr)
}

/// Stock string: `Cannot verify %1$s`.
pub(crate) async fn contact_not_verified(context: &Context, contact: &Contact) -> String {
    let addr = contact.get_name_n_addr();
    translated(context, StockMessage::ContactNotVerified)
        .await
        .replace1(addr)
}

/// Stock string: `Changed setup for %1$s`.
pub(crate) async fn contact_setup_changed(
    context: &Context,
    contact_addr: impl AsRef<str>,
) -> String {
    translated(context, StockMessage::ContactSetupChanged)
        .await
        .replace1(contact_addr)
}

/// Stock string: `Archived chats`.
pub(crate) async fn archived_chats(context: &Context) -> String {
    translated(context, StockMessage::ArchivedChats).await
}

/// Stock string: `Autocrypt Setup Message`.
pub(crate) async fn ac_setup_msg_subject(context: &Context) -> String {
    translated(context, StockMessage::AcSetupMsgSubject).await
}

/// Stock string: `This is the Autocrypt Setup Message used to transfer...`.
pub(crate) async fn ac_setup_msg_body(context: &Context) -> String {
    translated(context, StockMessage::AcSetupMsgBody).await
}

/// Stock string: `Multi Device Synchronization`.
pub(crate) async fn sync_msg_subject(context: &Context) -> String {
    translated(context, StockMessage::SyncMsgSubject).await
}

/// Stock string: `This message is used to synchronize data betweeen your devices.`.
pub(crate) async fn sync_msg_body(context: &Context) -> String {
    translated(context, StockMessage::SyncMsgBody).await
}

/// Stock string: `Cannot login as \"%1$s\". Please check...`.
pub(crate) async fn cannot_login(context: &Context, user: impl AsRef<str>) -> String {
    translated(context, StockMessage::CannotLogin)
        .await
        .replace1(user)
}

/// Stock string: `Location streaming enabled.`.
pub(crate) async fn msg_location_enabled(context: &Context) -> String {
    translated(context, StockMessage::MsgLocationEnabled).await
}

/// Stock string: `Location streaming enabled by ...`.
pub(crate) async fn msg_location_enabled_by(context: &Context, contact: ContactId) -> String {
    if contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEnabledLocation).await
    } else {
        translated(context, StockMessage::MsgLocationEnabledBy)
            .await
            .replace1(contact.get_stock_name(context).await)
    }
}

/// Stock string: `Location streaming disabled.`.
pub(crate) async fn msg_location_disabled(context: &Context) -> String {
    translated(context, StockMessage::MsgLocationDisabled).await
}

/// Stock string: `Location`.
pub(crate) async fn location(context: &Context) -> String {
    translated(context, StockMessage::Location).await
}

/// Stock string: `Sticker`.
pub(crate) async fn sticker(context: &Context) -> String {
    translated(context, StockMessage::Sticker).await
}

/// Stock string: `Device messages`.
pub(crate) async fn device_messages(context: &Context) -> String {
    translated(context, StockMessage::DeviceMessages).await
}

/// Stock string: `Saved messages`.
pub(crate) async fn saved_messages(context: &Context) -> String {
    translated(context, StockMessage::SavedMessages).await
}

/// Stock string: `Messages in this chat are generated locally by...`.
pub(crate) async fn device_messages_hint(context: &Context) -> String {
    translated(context, StockMessage::DeviceMessagesHint).await
}

/// Stock string: `Welcome to Delta Chat! – ...`.
pub(crate) async fn welcome_message(context: &Context) -> String {
    translated(context, StockMessage::WelcomeMessage).await
}

/// Stock string: `Unknown sender for this chat. See 'info' for more details.`.
pub(crate) async fn unknown_sender_for_chat(context: &Context) -> String {
    translated(context, StockMessage::UnknownSenderForChat).await
}

/// Stock string: `Message from %1$s`.
// TODO: This can compute `self_name` itself instead of asking the caller to do this.
pub(crate) async fn subject_for_new_contact(
    context: &Context,
    self_name: impl AsRef<str>,
) -> String {
    translated(context, StockMessage::SubjectForNewContact)
        .await
        .replace1(self_name)
}

/// Stock string: `Failed to send message to %1$s.`.
pub(crate) async fn failed_sending_to(context: &Context, name: impl AsRef<str>) -> String {
    translated(context, StockMessage::FailedSendingTo)
        .await
        .replace1(name)
}

/// Stock string: `Message deletion timer is disabled.`.
pub(crate) async fn msg_ephemeral_timer_disabled(
    context: &Context,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouDisabledEphemeralTimer).await
    } else {
        translated(context, StockMessage::MsgEphemeralTimerDisabledBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to %1$s s.`.
pub(crate) async fn msg_ephemeral_timer_enabled(
    context: &Context,
    timer: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEnabledEphemeralTimer)
            .await
            .replace1(timer)
    } else {
        translated(context, StockMessage::MsgEphemeralTimerEnabledBy)
            .await
            .replace1(timer)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to 1 minute.`.
pub(crate) async fn msg_ephemeral_timer_minute(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerMinute).await
    } else {
        translated(context, StockMessage::MsgEphemeralTimerMinuteBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to 1 hour.`.
pub(crate) async fn msg_ephemeral_timer_hour(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerHour).await
    } else {
        translated(context, StockMessage::MsgEphemeralTimerHourBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to 1 day.`.
pub(crate) async fn msg_ephemeral_timer_day(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerDay).await
    } else {
        translated(context, StockMessage::MsgEphemeralTimerDayBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to 1 week.`.
pub(crate) async fn msg_ephemeral_timer_week(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerWeek).await
    } else {
        translated(context, StockMessage::MsgEphemeralTimerWeekBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Video chat invitation`.
pub(crate) async fn videochat_invitation(context: &Context) -> String {
    translated(context, StockMessage::VideochatInvitation).await
}

/// Stock string: `You are invited to a video chat, click %1$s to join.`.
pub(crate) async fn videochat_invite_msg_body(context: &Context, url: impl AsRef<str>) -> String {
    translated(context, StockMessage::VideochatInviteMsgBody)
        .await
        .replace1(url)
}

/// Stock string: `Error:\n\n“%1$s”`.
pub(crate) async fn configuration_failed(context: &Context, details: impl AsRef<str>) -> String {
    translated(context, StockMessage::ConfigurationFailed)
        .await
        .replace1(details)
}

/// Stock string: `⚠️ Date or time of your device seem to be inaccurate (%1$s)...`.
// TODO: This could compute now itself.
pub(crate) async fn bad_time_msg_body(context: &Context, now: impl AsRef<str>) -> String {
    translated(context, StockMessage::BadTimeMsgBody)
        .await
        .replace1(now)
}

/// Stock string: `⚠️ Your Delta Chat version might be outdated...`.
pub(crate) async fn update_reminder_msg_body(context: &Context) -> String {
    translated(context, StockMessage::UpdateReminderMsgBody).await
}

/// Stock string: `Could not find your mail server...`.
pub(crate) async fn error_no_network(context: &Context) -> String {
    translated(context, StockMessage::ErrorNoNetwork).await
}

/// Stock string: `Chat protection enabled.`.
pub(crate) async fn protection_enabled(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::YouEnabledProtection).await
    } else {
        translated(context, StockMessage::ProtectionEnabledBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Chat protection disabled.`.
pub(crate) async fn protection_disabled(context: &Context, by_contact: ContactId) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::YouDisabledProtection).await
    } else {
        translated(context, StockMessage::ProtectionDisabledBy)
            .await
            .replace1(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Reply`.
pub(crate) async fn reply_noun(context: &Context) -> String {
    translated(context, StockMessage::ReplyNoun).await
}

/// Stock string: `You deleted the \"Saved messages\" chat...`.
pub(crate) async fn self_deleted_msg_body(context: &Context) -> String {
    translated(context, StockMessage::SelfDeletedMsgBody).await
}

/// Stock string: `⚠️ The "Delete messages from server" feature now also...`.
pub(crate) async fn delete_server_turned_off(context: &Context) -> String {
    translated(context, StockMessage::DeleteServerTurnedOff).await
}

/// Stock string: `Message deletion timer is set to %1$s minutes.`.
pub(crate) async fn msg_ephemeral_timer_minutes(
    context: &Context,
    minutes: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerMinutes)
            .await
            .replace1(minutes)
    } else {
        translated(context, StockMessage::MsgEphemeralTimerMinutesBy)
            .await
            .replace1(minutes)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to %1$s hours.`.
pub(crate) async fn msg_ephemeral_timer_hours(
    context: &Context,
    hours: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerHours)
            .await
            .replace1(hours)
    } else {
        translated(context, StockMessage::MsgEphemeralTimerHoursBy)
            .await
            .replace1(hours)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to %1$s days.`.
pub(crate) async fn msg_ephemeral_timer_days(
    context: &Context,
    days: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerDays)
            .await
            .replace1(days)
    } else {
        translated(context, StockMessage::MsgEphemeralTimerDaysBy)
            .await
            .replace1(days)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Message deletion timer is set to %1$s weeks.`.
pub(crate) async fn msg_ephemeral_timer_weeks(
    context: &Context,
    weeks: impl AsRef<str>,
    by_contact: ContactId,
) -> String {
    if by_contact == ContactId::SELF {
        translated(context, StockMessage::MsgYouEphemeralTimerWeeks)
            .await
            .replace1(weeks)
    } else {
        translated(context, StockMessage::MsgEphemeralTimerWeeksBy)
            .await
            .replace1(weeks)
            .replace2(by_contact.get_stock_name(context).await)
    }
}

/// Stock string: `Forwarded`.
pub(crate) async fn forwarded(context: &Context) -> String {
    translated(context, StockMessage::Forwarded).await
}

/// Stock string: `⚠️ Your provider's storage is about to exceed...`.
pub(crate) async fn quota_exceeding(context: &Context, highest_usage: u64) -> String {
    translated(context, StockMessage::QuotaExceedingMsgBody)
        .await
        .replace1(format!("{}", highest_usage))
        .replace("%%", "%")
}

/// Stock string: `%1$s message` with placeholder replaced by human-readable size.
pub(crate) async fn partial_download_msg_body(context: &Context, org_bytes: u32) -> String {
    let size = org_bytes
        .file_size(file_size_opts::BINARY)
        .unwrap_or_default();
    translated(context, StockMessage::PartialDownloadMsgBody)
        .await
        .replace1(size)
}

/// Stock string: `Download maximum available until %1$s`.
pub(crate) async fn download_availability(context: &Context, timestamp: i64) -> String {
    translated(context, StockMessage::DownloadAvailability)
        .await
        .replace1(timestamp_to_str(timestamp))
}

/// Stock string: `Incoming Messages`.
pub(crate) async fn incoming_messages(context: &Context) -> String {
    translated(context, StockMessage::IncomingMessages).await
}

/// Stock string: `Outgoing Messages`.
pub(crate) async fn outgoing_messages(context: &Context) -> String {
    translated(context, StockMessage::OutgoingMessages).await
}

/// Stock string: `Storage on %1$s`.
/// `%1$s` will be replaced by the domain of the configured email-address.
pub(crate) async fn storage_on_domain(context: &Context, domain: impl AsRef<str>) -> String {
    translated(context, StockMessage::StorageOnDomain)
        .await
        .replace1(domain)
}

/// Stock string: `Not connected`.
pub(crate) async fn not_connected(context: &Context) -> String {
    translated(context, StockMessage::NotConnected).await
}

/// Stock string: `Connected`.
pub(crate) async fn connected(context: &Context) -> String {
    translated(context, StockMessage::Connected).await
}

/// Stock string: `Connecting…`.
pub(crate) async fn connecting(context: &Context) -> String {
    translated(context, StockMessage::Connecting).await
}

/// Stock string: `Updating…`.
pub(crate) async fn updating(context: &Context) -> String {
    translated(context, StockMessage::Updating).await
}

/// Stock string: `Sending…`.
pub(crate) async fn sending(context: &Context) -> String {
    translated(context, StockMessage::Sending).await
}

/// Stock string: `Your last message was sent successfully.`.
pub(crate) async fn last_msg_sent_successfully(context: &Context) -> String {
    translated(context, StockMessage::LastMsgSentSuccessfully).await
}

/// Stock string: `Error: %1$s…`.
/// `%1$s` will be replaced by a possibly more detailed, typically english, error description.
pub(crate) async fn error(context: &Context, error: impl AsRef<str>) -> String {
    translated(context, StockMessage::Error)
        .await
        .replace1(error)
}

/// Stock string: `Not supported by your provider.`.
pub(crate) async fn not_supported_by_provider(context: &Context) -> String {
    translated(context, StockMessage::NotSupportedByProvider).await
}

/// Stock string: `Messages`.
/// Used as a subtitle in quota context; can be plural always.
pub(crate) async fn messages(context: &Context) -> String {
    translated(context, StockMessage::Messages).await
}

/// Stock string: `%1$s of %2$s used`.
pub(crate) async fn part_of_total_used(
    context: &Context,
    part: impl AsRef<str>,
    total: impl AsRef<str>,
) -> String {
    translated(context, StockMessage::PartOfTotallUsed)
        .await
        .replace1(part)
        .replace2(total)
}

/// Stock string: `Broadcast List`.
/// Used as the default name for broadcast lists; a number may be added.
pub(crate) async fn broadcast_list(context: &Context) -> String {
    translated(context, StockMessage::BroadcastList).await
}

/// Stock string: `%1$s changed their address from %2$s to %3$s`.
pub(crate) async fn aeap_addr_changed(
    context: &Context,
    contact_name: impl AsRef<str>,
    old_addr: impl AsRef<str>,
    new_addr: impl AsRef<str>,
) -> String {
    translated(context, StockMessage::AeapAddrChanged)
        .await
        .replace1(contact_name)
        .replace2(old_addr)
        .replace3(new_addr)
}

pub(crate) async fn aeap_explanation_and_link(
    context: &Context,
    old_addr: impl AsRef<str>,
    new_addr: impl AsRef<str>,
) -> String {
    translated(context, StockMessage::AeapExplanationAndLink)
        .await
        .replace1(old_addr)
        .replace2(new_addr)
}

impl Context {
    /// Set the stock string for the [StockMessage].
    ///
    pub async fn set_stock_translation(
        &self,
        id: StockMessage,
        stockstring: String,
    ) -> Result<(), Error> {
        if stockstring.contains("%1") && !id.fallback().contains("%1") {
            bail!(
                "translation {} contains invalid %1 placeholder, default is {}",
                stockstring,
                id.fallback()
            );
        }
        if stockstring.contains("%2") && !id.fallback().contains("%2") {
            bail!(
                "translation {} contains invalid %2 placeholder, default is {}",
                stockstring,
                id.fallback()
            );
        }
        self.translated_stockstrings
            .write()
            .await
            .insert(id as usize, stockstring);
        Ok(())
    }

    /// Returns a stock message saying that protection status has changed.
    pub(crate) async fn stock_protection_msg(
        &self,
        protect: ProtectionStatus,
        from_id: ContactId,
    ) -> String {
        match protect {
            ProtectionStatus::Unprotected => protection_enabled(self, from_id).await,
            ProtectionStatus::Protected => protection_disabled(self, from_id).await,
        }
    }

    pub(crate) async fn update_device_chats(&self) -> Result<(), Error> {
        if self.get_config_bool(Config::Bot).await? {
            return Ok(());
        }

        // create saved-messages chat; we do this only once, if the user has deleted the chat,
        // he can recreate it manually (make sure we do not re-add it when configure() was called a second time)
        if !self.sql.get_raw_config_bool("self-chat-added").await? {
            self.sql
                .set_raw_config_bool("self-chat-added", true)
                .await?;
            ChatId::create_for_contact(self, ContactId::SELF).await?;
        }

        // add welcome-messages. by the label, this is done only once,
        // if the user has deleted the message or the chat, it is not added again.
        let image = include_bytes!("../assets/welcome-image.jpg");
        let blob = BlobObject::create(self, "welcome-image.jpg", image).await?;
        let mut msg = Message::new(Viewtype::Image);
        msg.param.set(Param::File, blob.as_name());
        chat::add_device_msg(self, Some("core-welcome-image"), Some(&mut msg)).await?;

        let mut msg = Message::new(Viewtype::Text);
        msg.text = Some(welcome_message(self).await);
        chat::add_device_msg(self, Some("core-welcome"), Some(&mut msg)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use num_traits::ToPrimitive;

    use crate::chat::Chat;
    use crate::chatlist::Chatlist;
    use crate::test_utils::TestContext;

    use super::*;

    #[test]
    fn test_enum_mapping() {
        assert_eq!(StockMessage::NoMessages.to_usize().unwrap(), 1);
        assert_eq!(StockMessage::SelfMsg.to_usize().unwrap(), 2);
    }

    #[test]
    fn test_fallback() {
        assert_eq!(StockMessage::NoMessages.fallback(), "No messages.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_set_stock_translation() {
        let t = TestContext::new().await;
        t.set_stock_translation(StockMessage::NoMessages, "xyz".to_string())
            .await
            .unwrap();
        assert_eq!(no_messages(&t).await, "xyz")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_set_stock_translation_wrong_replacements() {
        let t = TestContext::new().await;
        assert!(t
            .ctx
            .set_stock_translation(StockMessage::NoMessages, "xyz %1$s ".to_string())
            .await
            .is_err());
        assert!(t
            .ctx
            .set_stock_translation(StockMessage::NoMessages, "xyz %2$s ".to_string())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stock_str() {
        let t = TestContext::new().await;
        assert_eq!(no_messages(&t).await, "No messages.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stock_string_repl_str() {
        let t = TestContext::new().await;
        let contact_id = Contact::create(&t.ctx, "Someone", "someone@example.org")
            .await
            .unwrap();
        let contact = Contact::load_from_db(&t.ctx, contact_id).await.unwrap();
        // uses %1$s substitution
        assert_eq!(
            contact_verified(&t, &contact).await,
            "Someone (someone@example.org) verified."
        );
        // We have no string using %1$d to test...
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stock_system_msg_simple() {
        let t = TestContext::new().await;
        assert_eq!(
            msg_location_enabled(&t).await,
            "Location streaming enabled."
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stock_system_msg_add_member_by_me() {
        let t = TestContext::new().await;
        assert_eq!(
            msg_add_member(&t, "alice@example.org", ContactId::SELF).await,
            "You added member alice@example.org."
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stock_system_msg_add_member_by_me_with_displayname() {
        let t = TestContext::new().await;
        Contact::create(&t, "Alice", "alice@example.org")
            .await
            .expect("failed to create contact");
        assert_eq!(
            msg_add_member(&t, "alice@example.org", ContactId::SELF).await,
            "You added member Alice (alice@example.org)."
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stock_system_msg_add_member_by_other_with_displayname() {
        let t = TestContext::new().await;
        let contact_id = {
            Contact::create(&t, "Alice", "alice@example.org")
                .await
                .expect("Failed to create contact Alice");
            Contact::create(&t, "Bob", "bob@example.com")
                .await
                .expect("failed to create bob")
        };
        assert_eq!(
            msg_add_member(&t, "alice@example.org", contact_id,).await,
            "Member Alice (alice@example.org) added by Bob (bob@example.com)."
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_quota_exceeding_stock_str() -> anyhow::Result<()> {
        let t = TestContext::new().await;
        let str = quota_exceeding(&t, 81).await;
        assert!(str.contains("81% "));
        assert!(str.contains("100% "));
        assert!(!str.contains("%%"));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_partial_download_msg_body() -> anyhow::Result<()> {
        let t = TestContext::new().await;
        let str = partial_download_msg_body(&t, 1024 * 1024).await;
        assert_eq!(str, "1 MiB message");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_update_device_chats() {
        let t = TestContext::new().await;
        t.update_device_chats().await.ok();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 2);

        let chat0 = Chat::load_from_db(&t, chats.get_chat_id(0).unwrap())
            .await
            .unwrap();
        let (self_talk_id, device_chat_id) = if chat0.is_self_talk() {
            (chats.get_chat_id(0).unwrap(), chats.get_chat_id(1).unwrap())
        } else {
            (chats.get_chat_id(1).unwrap(), chats.get_chat_id(0).unwrap())
        };

        // delete self-talk first; this adds a message to device-chat about how self-talk can be restored
        let device_chat_msgs_before = chat::get_chat_msgs(&t, device_chat_id, 0)
            .await
            .unwrap()
            .len();
        self_talk_id.delete(&t).await.ok();
        assert_eq!(
            chat::get_chat_msgs(&t, device_chat_id, 0)
                .await
                .unwrap()
                .len(),
            device_chat_msgs_before + 1
        );

        // delete device chat
        device_chat_id.delete(&t).await.ok();

        // check, that the chatlist is empty
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0);

        // a subsequent call to update_device_chats() must not re-add manally deleted messages or chats
        t.update_device_chats().await.ok();
        let chats = Chatlist::try_load(&t, 0, None, None).await.unwrap();
        assert_eq!(chats.len(), 0);
    }
}
