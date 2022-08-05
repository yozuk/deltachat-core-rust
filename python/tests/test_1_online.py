import os
import queue
import sys
from datetime import datetime, timezone

import pytest
from imap_tools import AND, U

from deltachat import const
from deltachat.hookspec import account_hookimpl
from deltachat.message import Message
from deltachat.tracker import ImexTracker


def test_basic_imap_api(acfactory, tmpdir):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat12 = acfactory.get_accepted_chat(ac1, ac2)

    imap2 = ac2.direct_imap

    with imap2.idle() as idle2:
        chat12.send_text("hello")
        ac2._evtracker.wait_next_incoming_message()
        idle2.wait_for_new_message()

    assert imap2.get_unread_cnt() == 1
    imap2.mark_all_read()
    assert imap2.get_unread_cnt() == 0

    imap2.dump_imap_structures(tmpdir, logfile=sys.stdout)
    imap2.shutdown()


@pytest.mark.ignored
def test_configure_generate_key(acfactory, lp):
    # A slow test which will generate new keys.
    acfactory.remove_preconfigured_keys()
    ac1 = acfactory.new_online_configuring_account(key_gen_type=str(const.DC_KEY_GEN_RSA2048))
    ac2 = acfactory.new_online_configuring_account(key_gen_type=str(const.DC_KEY_GEN_ED25519))
    acfactory.bring_accounts_online()
    chat = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("ac1: send unencrypted message to ac2")
    chat.send_text("message1")
    lp.sec("ac2: waiting for message from ac1")
    msg_in = ac2._evtracker.wait_next_incoming_message()
    assert msg_in.text == "message1"
    assert not msg_in.is_encrypted()

    lp.sec("ac2: send encrypted message to ac1")
    msg_in.chat.send_text("message2")
    lp.sec("ac1: waiting for message from ac2")
    msg2_in = ac1._evtracker.wait_next_incoming_message()
    assert msg2_in.text == "message2"
    assert msg2_in.is_encrypted()

    lp.sec("ac1: send encrypted message to ac2")
    msg2_in.chat.send_text("message3")
    lp.sec("ac2: waiting for message from ac1")
    msg3_in = ac2._evtracker.wait_next_incoming_message()
    assert msg3_in.text == "message3"
    assert msg3_in.is_encrypted()


def test_configure_canceled(acfactory):
    ac1 = acfactory.new_online_configuring_account()
    ac1.stop_ongoing()
    try:
        acfactory.wait_configured(ac1)
    except pytest.fail.Exception:
        pass


def test_export_import_self_keys(acfactory, tmpdir, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)

    dir = tmpdir.mkdir("exportdir")
    export_files = ac1.export_self_keys(dir.strpath)
    assert len(export_files) == 2
    for x in export_files:
        assert x.startswith(dir.strpath)
    (key_id,) = ac1._evtracker.get_info_regex_groups(r".*xporting.*KeyId\((.*)\).*")
    ac1._evtracker.consume_events()

    lp.sec("exported keys (private and public)")
    for name in os.listdir(dir.strpath):
        lp.indent(dir.strpath + os.sep + name)
    lp.sec("importing into existing account")
    ac2.import_self_keys(dir.strpath)
    (key_id2,) = ac2._evtracker.get_info_regex_groups(r".*stored.*KeyId\((.*)\).*", check_error=False)
    assert key_id2 == key_id


def test_one_account_send_bcc_setting(acfactory, lp):
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account()
    ac1_clone = acfactory.new_online_configuring_account(cloned_from=ac1)
    acfactory.bring_accounts_online()

    # test if sent messages are copied to it via BCC.

    chat = acfactory.get_accepted_chat(ac1, ac2)
    self_addr = ac1.get_config("addr")
    other_addr = ac2.get_config("addr")

    lp.sec("send out message without bcc to ourselves")
    ac1.set_config("bcc_self", "0")
    msg_out = chat.send_text("message1")
    assert not msg_out.is_forwarded()

    # wait for send out (no BCC)
    ev = ac1._evtracker.get_matching("DC_EVENT_SMTP_MESSAGE_SENT")
    assert ac1.get_config("bcc_self") == "0"

    # make sure we are not sending message to ourselves
    assert self_addr not in ev.data2
    assert other_addr in ev.data2

    lp.sec("ac1: setting bcc_self=1")
    ac1.set_config("bcc_self", "1")

    lp.sec("send out message with bcc to ourselves")
    with ac1.direct_imap.idle() as idle1:
        msg_out = chat.send_text("message2")

        # wait for send out (BCC)
        ev = ac1._evtracker.get_matching("DC_EVENT_SMTP_MESSAGE_SENT")
        assert ac1.get_config("bcc_self") == "1"

        # now make sure we are sending message to ourselves too
        assert self_addr in ev.data2
        assert other_addr in ev.data2
        assert idle1.wait_for_seen()

    # Second client receives only second message, but not the first
    ev_msg = ac1_clone._evtracker.wait_next_messages_changed()
    assert ev_msg.text == msg_out.text


def test_send_file_twice_unicode_filename_mangling(tmpdir, acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)

    basename = "somedäüta.html.zip"
    p = os.path.join(tmpdir.strpath, basename)
    with open(p, "w") as f:
        f.write("some data")

    def send_and_receive_message():
        lp.sec("ac1: prepare and send attachment + text to ac2")
        msg1 = Message.new_empty(ac1, "file")
        msg1.set_text("withfile")
        msg1.set_file(p)
        chat.send_msg(msg1)

        lp.sec("ac2: receive message")
        ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG")
        assert ev.data2 > const.DC_CHAT_ID_LAST_SPECIAL
        return ac2.get_message_by_id(ev.data2)

    msg = send_and_receive_message()
    assert msg.text == "withfile"
    assert open(msg.filename).read() == "some data"
    assert msg.filename.endswith(basename)

    msg2 = send_and_receive_message()
    assert msg2.text == "withfile"
    assert open(msg2.filename).read() == "some data"
    assert msg2.filename.endswith("html.zip")
    assert msg.filename != msg2.filename


def test_send_file_html_attachment(tmpdir, acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)

    basename = "test.html"
    content = "<html><body>text</body>data"

    p = os.path.join(tmpdir.strpath, basename)
    with open(p, "w") as f:
        # write wrong html to see if core tries to parse it
        # (it shouldn't as it's a file attachment)
        f.write(content)

    lp.sec("ac1: prepare and send attachment + text to ac2")
    chat.send_file(p, mime_type="text/html")

    lp.sec("ac2: receive message")
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG")
    assert ev.data2 > const.DC_CHAT_ID_LAST_SPECIAL
    msg = ac2.get_message_by_id(ev.data2)

    assert open(msg.filename).read() == content
    assert msg.filename.endswith(basename)


def test_html_message(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)
    html_text = "<p>hello HTML world</p>"

    lp.sec("ac1: prepare and send text message to ac2")
    msg1 = chat.send_text("message0")
    assert not msg1.has_html()
    assert msg1.html == ""

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message0"
    assert not msg2.has_html()
    assert msg2.html == ""

    lp.sec("ac1: prepare and send HTML+text message to ac2")
    msg1 = Message.new_empty(ac1, "text")
    msg1.set_text("message1")
    msg1.set_html(html_text)
    msg1 = chat.send_msg(msg1)
    assert msg1.has_html()
    assert html_text in msg1.html

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message1"
    assert msg2.has_html()
    assert html_text in msg2.html

    lp.sec("ac1: prepare and send HTML-only message to ac2")
    msg1 = Message.new_empty(ac1, "text")
    msg1.set_html(html_text)
    msg1 = chat.send_msg(msg1)

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert "<p>" not in msg2.text
    assert "hello HTML world" in msg2.text
    assert msg2.has_html()
    assert html_text in msg2.html


def test_videochat_invitation_message(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)
    text = "You are invited to a video chat, click https://meet.jit.si/WxEGad0gGzX to join."

    lp.sec("ac1: prepare and send text message to ac2")
    msg1 = chat.send_text("message0")
    assert not msg1.is_videochat_invitation()

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message0"
    assert not msg2.is_videochat_invitation()

    lp.sec("ac1: prepare and send videochat invitation to ac2")
    msg1 = Message.new_empty(ac1, "videochat")
    msg1.set_text(text)
    msg1 = chat.send_msg(msg1)
    assert msg1.is_videochat_invitation()

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == text
    assert msg2.is_videochat_invitation()


def test_webxdc_message(acfactory, data, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("ac1: prepare and send text message to ac2")
    msg1 = chat.send_text("message0")
    assert not msg1.is_webxdc()
    assert not msg1.send_status_update({"payload": "not an webxdc"}, "invalid")
    assert not msg1.get_status_updates()

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message0"
    assert not msg2.is_webxdc()
    assert not msg1.get_status_updates()

    lp.sec("ac1: prepare and send webxdc instance to ac2")
    msg1 = Message.new_empty(ac1, "webxdc")
    msg1.set_text("message1")
    msg1.set_file(data.get_path("webxdc/minimal.xdc"))
    msg1 = chat.send_msg(msg1)
    assert msg1.is_webxdc()
    assert msg1.filename

    assert msg1.send_status_update({"payload": "test1"}, "some test data")
    assert msg1.send_status_update({"payload": "test2"}, "more test data")
    assert len(msg1.get_status_updates()) == 2
    update1 = msg1.get_status_updates()[0]
    assert update1["payload"] == "test1"
    assert len(msg1.get_status_updates(update1["serial"])) == 1

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message1"
    assert msg2.is_webxdc()
    assert msg2.filename


def test_mvbox_sentbox_threads(acfactory, lp):
    lp.sec("ac1: start with mvbox thread")
    ac1 = acfactory.new_online_configuring_account(mvbox_move=True, sentbox_watch=True)

    lp.sec("ac2: start without mvbox/sentbox threads")
    ac2 = acfactory.new_online_configuring_account(mvbox_move=False, sentbox_watch=False)

    lp.sec("ac2 and ac1: waiting for configuration")
    acfactory.bring_accounts_online()

    lp.sec("ac1: send message and wait for ac2 to receive it")
    acfactory.get_accepted_chat(ac1, ac2).send_text("message1")
    assert ac2._evtracker.wait_next_incoming_message().text == "message1"


def test_move_works(acfactory):
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account(mvbox_move=True)
    acfactory.bring_accounts_online()
    chat = acfactory.get_accepted_chat(ac1, ac2)
    chat.send_text("message1")

    # Message is moved to the movebox
    ac2._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_MOVED")

    # Message is downloaded
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG")
    assert ev.data2 > const.DC_CHAT_ID_LAST_SPECIAL


def test_move_works_on_self_sent(acfactory):
    ac1 = acfactory.new_online_configuring_account(mvbox_move=True)
    ac2 = acfactory.new_online_configuring_account()
    acfactory.bring_accounts_online()
    ac1.set_config("bcc_self", "1")

    chat = acfactory.get_accepted_chat(ac1, ac2)
    chat.send_text("message1")
    ac1._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_MOVED")
    chat.send_text("message2")
    ac1._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_MOVED")
    chat.send_text("message3")
    ac1._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_MOVED")


def test_forward_messages(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = ac1.create_chat(ac2)

    lp.sec("ac1: send message to ac2")
    msg_out = chat.send_text("message2")

    lp.sec("ac2: wait for receive")
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG|DC_EVENT_MSGS_CHANGED")
    assert ev.data2 == msg_out.id
    msg_in = ac2.get_message_by_id(msg_out.id)
    assert msg_in.text == "message2"

    lp.sec("ac2: check that the message arrived in a chat")
    chat2 = msg_in.chat
    assert msg_in in chat2.get_messages()
    assert not msg_in.is_forwarded()
    assert chat2.is_contact_request()

    lp.sec("ac2: create new chat and forward message to it")
    chat3 = ac2.create_group_chat("newgroup")
    assert not chat3.is_promoted()
    ac2.forward_messages([msg_in], chat3)

    lp.sec("ac2: check new chat has a forwarded message")
    assert chat3.is_promoted()
    messages = chat3.get_messages()
    msg = messages[-1]
    assert msg.is_forwarded()
    ac2.delete_messages(messages)
    assert not chat3.get_messages()


def test_forward_own_message(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("sending message")
    msg_out = chat.send_text("message2")

    lp.sec("receiving message")
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG")
    msg_in = ac2.get_message_by_id(ev.data2)
    assert msg_in.text == "message2"
    assert not msg_in.is_forwarded()

    lp.sec("ac1: creating group chat, and forward own message")
    group = ac1.create_group_chat("newgroup2")
    group.add_contact(ac2)
    ac1.forward_messages([msg_out], group)

    # wait for other account to receive
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG")
    msg_in = ac2.get_message_by_id(ev.data2)
    assert msg_in.text == "message2"
    assert msg_in.is_forwarded()


def test_send_self_message(acfactory, lp):
    ac1 = acfactory.new_online_configuring_account(mvbox_move=True)
    acfactory.bring_accounts_online()
    lp.sec("ac1: create self chat")
    chat = ac1.get_self_contact().create_chat()
    chat.send_text("hello")
    ac1._evtracker.get_matching("DC_EVENT_SMTP_MESSAGE_SENT")


def test_send_dot(acfactory, lp):
    """Test that a single dot is properly escaped in SMTP protocol"""
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("sending message")
    msg_out = chat.send_text(".")

    lp.sec("receiving message")
    msg_in = ac2._evtracker.wait_next_incoming_message()
    assert msg_in.text == msg_out.text


def test_send_and_receive_message_markseen(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)

    # make DC's life harder wrt to encodings
    ac1.set_config("displayname", "ä name")

    # clear any fresh device messages
    ac1.get_device_chat().mark_noticed()
    ac2.get_device_chat().mark_noticed()

    lp.sec("ac1: create chat with ac2")
    chat = ac1.create_chat(ac2)

    lp.sec("sending text message from ac1 to ac2")
    msg1 = chat.send_text("message1")
    ac1._evtracker.wait_msg_delivered(msg1)

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message1"
    assert not msg2.is_forwarded()
    assert msg2.get_sender_contact().display_name == ac1.get_config("displayname")

    lp.sec("check the message arrived in contact request chat")
    chat2 = msg2.chat
    assert msg2 in chat2.get_messages()
    assert chat2.is_contact_request()
    assert chat2.count_fresh_messages() == 1
    # Like it or not, this assert is flaky
    # assert msg2.time_received >= msg1.time_sent

    lp.sec("create new chat with contact and verify it's proper")
    chat2b = msg2.create_chat()
    assert not chat2b.is_contact_request()
    assert chat2b.count_fresh_messages() == 1

    lp.sec("mark chat as noticed")
    chat2b.mark_noticed()
    assert chat2b.count_fresh_messages() == 0

    ac2._evtracker.consume_events()

    lp.sec("sending a second message from ac1 to ac2")
    msg3 = chat.send_text("message2")

    lp.sec("wait for ac2 to receive second message")
    msg4 = ac2._evtracker.wait_next_incoming_message()

    lp.sec("mark messages as seen on ac2, wait for changes on ac1")
    with ac1.direct_imap.idle() as idle1:
        with ac2.direct_imap.idle() as idle2:
            ac2.mark_seen_messages([msg2, msg4])
            ev = ac2._evtracker.get_matching("DC_EVENT_MSGS_NOTICED")
            assert msg2.chat.id == msg4.chat.id
            assert ev.data1 == msg2.chat.id
            assert ev.data2 == 0
            idle2.wait_for_seen()

        lp.step("1")
        for i in range(2):
            ev = ac1._evtracker.get_matching("DC_EVENT_MSG_READ")
            assert ev.data1 > const.DC_CHAT_ID_LAST_SPECIAL
            assert ev.data2 > const.DC_MSG_ID_LAST_SPECIAL
        lp.step("2")
        idle1.wait_for_seen()  # Check that ac1 marks the read receipt as read

    assert msg1.is_out_mdn_received()
    assert msg3.is_out_mdn_received()

    lp.sec("try check that a second call to mark_seen doesn't happen")
    ac2._evtracker.consume_events()
    msg2.mark_seen()
    try:
        ac2._evtracker.get_matching("DC_EVENT_MSG_READ", timeout=0.01)
    except queue.Empty:
        pass  # mark_seen_messages() has generated events before it returns


def test_moved_markseen(acfactory, lp):
    """Test that message already moved to DeltaChat folder is marked as seen."""
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account(mvbox_move=True)
    acfactory.bring_accounts_online()

    ac2.stop_io()
    with ac2.direct_imap.idle() as idle2:
        ac1.create_chat(ac2).send_text("Hello!")
        idle2.wait_for_new_message()

    # Emulate moving of the message to DeltaChat folder by Sieve rule.
    ac2.direct_imap.conn.move(["*"], "DeltaChat")
    ac2.direct_imap.select_folder("DeltaChat")

    with ac2.direct_imap.idle() as idle2:
        ac2.start_io()
        msg = ac2._evtracker.wait_next_incoming_message()

        # Accept the contact request.
        msg.chat.accept()
        ac2.mark_seen_messages([msg])
        uid = idle2.wait_for_seen()

    assert len([a for a in ac2.direct_imap.conn.fetch(AND(seen=True, uid=U(uid, "*")))]) == 1


def test_message_override_sender_name(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    ac1.set_config("displayname", "ac1-default-displayname")
    chat = acfactory.get_accepted_chat(ac1, ac2)
    overridden_name = "someone else"

    lp.sec("sending text message with overridden name from ac1 to ac2")
    msg1 = Message.new_empty(ac1, "text")
    msg1.set_override_sender_name(overridden_name)
    msg1.set_text("message1")
    msg1 = chat.send_msg(msg1)
    assert msg1.override_sender_name == overridden_name

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message1"
    sender = msg2.get_sender_contact()
    assert sender.addr == ac1.get_config("addr")
    assert sender.name == ac1.get_config("displayname")
    assert msg2.override_sender_name == overridden_name

    lp.sec("sending normal text message from ac1 to ac2")
    msg1 = Message.new_empty(ac1, "text")
    msg1.set_text("message2")
    msg1 = chat.send_msg(msg1)
    assert not msg1.override_sender_name

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message2"
    assert msg2.get_sender_contact().name == ac1.get_config("displayname")
    assert not msg2.override_sender_name


@pytest.mark.parametrize("mvbox_move", [True, False])
def test_markseen_message_and_mdn(acfactory, mvbox_move):
    # Please only change this test if you are very sure that it will still catch the issues it catches now.
    # We had so many problems with markseen, if in doubt, rather create another test, it can't harm.
    ac1 = acfactory.new_online_configuring_account(mvbox_move=mvbox_move)
    ac2 = acfactory.new_online_configuring_account(mvbox_move=mvbox_move)
    acfactory.bring_accounts_online()
    # Do not send BCC to self, we only want to test MDN on ac1.
    ac1.set_config("bcc_self", "0")

    folder = "mvbox" if mvbox_move else "inbox"
    ac1.direct_imap.select_config_folder(folder)
    ac2.direct_imap.select_config_folder(folder)
    with ac1.direct_imap.idle() as idle1:
        with ac2.direct_imap.idle() as idle2:
            acfactory.get_accepted_chat(ac1, ac2).send_text("hi")
            msg = ac2._evtracker.wait_next_incoming_message()

            ac2.mark_seen_messages([msg])

            idle2.wait_for_seen()  # Check original message is marked as seen
            idle1.wait_for_seen()  # Check that the mdn is marked as seen


def test_reply_privately(acfactory):
    ac1, ac2 = acfactory.get_online_accounts(2)

    group1 = ac1.create_group_chat("group")
    group1.add_contact(ac2)
    group1.send_text("hello")

    msg2 = ac2._evtracker.wait_next_incoming_message()
    group2 = msg2.create_chat()
    assert group2.get_name() == group1.get_name()

    msg_reply = Message.new_empty(ac2, "text")
    msg_reply.set_text("message reply")
    msg_reply.quote = msg2

    private_chat1 = ac1.create_chat(ac2)
    private_chat2 = ac2.create_chat(ac1)
    private_chat2.send_msg(msg_reply)

    msg_reply1 = ac1._evtracker.wait_next_incoming_message()
    assert msg_reply1.quoted_text == "hello"
    assert not msg_reply1.chat.is_group()
    assert msg_reply1.chat.id == private_chat1.id


def test_mdn_asymmetric(acfactory, lp):
    ac1 = acfactory.new_online_configuring_account(mvbox_move=True)
    ac2 = acfactory.new_online_configuring_account()
    acfactory.bring_accounts_online()

    lp.sec("ac1: create chat with ac2")
    chat = ac1.create_chat(ac2)
    ac2.create_chat(ac1)

    # make sure mdns are enabled (usually enabled by default already)
    ac1.set_config("mdns_enabled", "1")
    ac2.set_config("mdns_enabled", "1")

    lp.sec("sending text message from ac1 to ac2")
    msg_out = chat.send_text("message1")

    assert len(chat.get_messages()) == 1

    lp.sec("disable ac1 MDNs")
    ac1.set_config("mdns_enabled", "0")

    lp.sec("wait for ac2 to receive message")
    msg = ac2._evtracker.wait_next_incoming_message()

    assert len(msg.chat.get_messages()) == 1

    ac1.direct_imap.select_config_folder("mvbox")
    with ac1.direct_imap.idle() as idle1:
        lp.sec("ac2: mark incoming message as seen")
        ac2.mark_seen_messages([msg])

        lp.sec("ac1: waiting for incoming activity")
        # MDN should be moved even though MDNs are already disabled
        ac1._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_MOVED")

        assert len(chat.get_messages()) == 1

        # Wait for the message to be marked as seen on IMAP.
        assert idle1.wait_for_seen()

    # MDN is received even though MDNs are already disabled
    assert msg_out.is_out_mdn_received()


def test_send_and_receive_will_encrypt_decrypt(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)

    ac1.get_device_chat().mark_noticed()

    lp.sec("ac1: create chat with ac2")
    chat = ac1.create_chat(ac2)

    lp.sec("sending text message from ac1 to ac2")
    chat.send_text("message1")

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message1"

    lp.sec("create new chat with contact and send back (encrypted) message")
    chat2b = msg2.create_chat()
    chat2b.send_text("message-back")

    lp.sec("wait for ac1 to receive message")
    msg3 = ac1._evtracker.wait_next_incoming_message()
    assert msg3.text == "message-back"
    assert msg3.is_encrypted() and msg3.is_in_fresh()

    # test get_fresh_messages
    fresh_msgs = list(ac1.get_fresh_messages())
    assert len(fresh_msgs) == 1
    assert fresh_msgs[0] == msg3
    msg3.mark_seen()
    assert not list(ac1.get_fresh_messages())

    # Test that we do not gossip peer keys in 1-to-1 chat,
    # as it makes no sense to gossip to peers their own keys.
    # Gossip is only sent in encrypted messages,
    # and we sent encrypted msg_back right above.
    assert chat2b.get_summary()["gossiped_timestamp"] == 0

    lp.sec("create group chat with two members, one of which has no encrypt state")
    chat = ac1.create_group_chat("encryption test")
    chat.add_contact(ac2)
    chat.add_contact(ac1.create_contact("notexisting@testrun.org"))
    msg = chat.send_text("test not encrypt")
    assert not msg.is_encrypted()
    ac1._evtracker.get_matching("DC_EVENT_SMTP_MESSAGE_SENT")


def test_gossip_optimization(acfactory, lp):
    """Test that gossip timestamp is updated when someone else sends gossip,
    so we don't have to send gossip ourselves.
    """
    ac1, ac2, ac3 = acfactory.get_online_accounts(3)

    acfactory.introduce_each_other([ac1, ac2])
    acfactory.introduce_each_other([ac2, ac3])

    lp.sec("ac1 creates a group chat with ac2")
    group_chat = ac1.create_group_chat("hello")
    group_chat.add_contact(ac2)
    msg = group_chat.send_text("hi")

    # No Autocrypt gossip was sent yet.
    gossiped_timestamp = msg.chat.get_summary()["gossiped_timestamp"]
    assert gossiped_timestamp == 0

    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.is_encrypted()
    assert msg.text == "hi"

    lp.sec("ac2 adds ac3 to the group")
    msg.chat.add_contact(ac3)

    lp.sec("ac1 receives message from ac2 and updates gossip timestamp")
    msg = ac1._evtracker.wait_next_incoming_message()
    assert msg.is_encrypted()

    # ac1 has updated the gossip timestamp even though no gossip was sent by ac1.
    # ac1 does not need to send gossip because ac2 already did it.
    gossiped_timestamp = msg.chat.get_summary()["gossiped_timestamp"]
    assert gossiped_timestamp == int(msg.time_sent.timestamp())


def test_gossip_encryption_preference(acfactory, lp):
    """Test that encryption preference of group members is gossiped to new members.
    This is a Delta Chat extension to Autocrypt 1.1.0, which Autocrypt-Gossip headers
    SHOULD NOT contain encryption preference.
    """
    ac1, ac2, ac3 = acfactory.get_online_accounts(3)

    lp.sec("ac1 learns that ac2 prefers encryption")
    ac1.create_chat(ac2)
    msg = ac2.create_chat(ac1).send_text("first message")
    msg = ac1._evtracker.wait_next_incoming_message()
    assert msg.text == "first message"
    assert not msg.is_encrypted()
    res = "End-to-end encryption preferred:\n{}".format(ac2.get_config("addr"))
    assert msg.chat.get_encryption_info() == res
    lp.sec("ac2 learns that ac3 prefers encryption")
    ac2.create_chat(ac3)
    msg = ac3.create_chat(ac2).send_text("I prefer encryption")
    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.text == "I prefer encryption"
    assert not msg.is_encrypted()

    lp.sec("ac3 does not know that ac1 prefers encryption")
    ac1.create_chat(ac3)
    chat = ac3.create_chat(ac1)
    res = "No encryption:\n{}".format(ac1.get_config("addr"))
    assert chat.get_encryption_info() == res
    msg = chat.send_text("not encrypted")
    msg = ac1._evtracker.wait_next_incoming_message()
    assert msg.text == "not encrypted"
    assert not msg.is_encrypted()

    lp.sec("ac1 creates a group chat with ac2")
    group_chat = ac1.create_group_chat("hello")
    group_chat.add_contact(ac2)
    encryption_info = group_chat.get_encryption_info()
    res = "End-to-end encryption preferred:\n{}".format(ac2.get_config("addr"))
    assert encryption_info == res
    msg = group_chat.send_text("hi")

    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.is_encrypted()
    assert msg.text == "hi"

    lp.sec("ac2 adds ac3 to the group")
    msg.chat.add_contact(ac3)
    assert msg.is_encrypted()

    lp.sec("ac3 learns that ac1 prefers encryption")
    msg = ac3._evtracker.wait_next_incoming_message()
    encryption_info = msg.chat.get_encryption_info().splitlines()
    assert encryption_info[0] == "End-to-end encryption preferred:"
    assert ac1.get_config("addr") in encryption_info[1:]
    assert ac2.get_config("addr") in encryption_info[1:]
    msg = chat.send_text("encrypted")
    assert msg.is_encrypted()


def test_send_first_message_as_long_unicode_with_cr(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    ac2.set_config("save_mime_headers", "1")

    lp.sec("ac1: create chat with ac2")
    chat = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("sending multi-line non-unicode message from ac1 to ac2")
    text1 = (
        "hello\nworld\nthis is a very long message that should be"
        + " wrapped using format=flowed and unwrapped on the receiver"
    )
    msg_out = chat.send_text(text1)
    assert not msg_out.is_encrypted()

    lp.sec("wait for ac2 to receive multi-line non-unicode message")
    msg_in = ac2._evtracker.wait_next_incoming_message()
    assert msg_in.text == text1

    lp.sec("sending multi-line unicode text message from ac1 to ac2")
    text2 = "äalis\nthis is ßßÄ"
    msg_out = chat.send_text(text2)
    assert not msg_out.is_encrypted()

    lp.sec("wait for ac2 to receive multi-line unicode message")
    msg_in = ac2._evtracker.wait_next_incoming_message()
    assert msg_in.text == text2
    assert ac1.get_config("addr") in [x.addr for x in msg_in.chat.get_contacts()]


def test_no_draft_if_cant_send(acfactory):
    """Tests that no quote can be set if the user can't send to this chat"""
    (ac1,) = acfactory.get_online_accounts(1)
    device_chat = ac1.get_device_chat()
    msg = Message.new_empty(ac1, "text")
    device_chat.set_draft(msg)

    assert not device_chat.can_send()
    assert device_chat.get_draft() is None


def test_dont_show_emails(acfactory, lp):
    """Most mailboxes have a "Drafts" folder where constantly new emails appear but we don't actually want to show them.
    So: If it's outgoing AND there is no Received header AND it's not in the sentbox, then ignore the email.

    If the draft email is sent out later (i.e. moved to "Sent"), it must be shown.

    Also, test that unknown emails in the Spam folder are not shown."""
    ac1 = acfactory.new_online_configuring_account()
    ac1.set_config("show_emails", "2")
    ac1.create_contact("alice@example.org").create_chat()

    acfactory.wait_configured(ac1)
    ac1.direct_imap.create_folder("Drafts")
    ac1.direct_imap.create_folder("Sent")
    ac1.direct_imap.create_folder("Spam")
    ac1.direct_imap.create_folder("Junk")

    acfactory.bring_accounts_online()
    ac1.stop_io()

    ac1.direct_imap.append(
        "Drafts",
        """
        From: ac1 <{}>
        Subject: subj
        To: alice@example.org
        Message-ID: <aepiors@example.org>
        Content-Type: text/plain; charset=utf-8

        message in Drafts that is moved to Sent later
    """.format(
            ac1.get_config("configured_addr")
        ),
    )
    ac1.direct_imap.append(
        "Sent",
        """
        From: ac1 <{}>
        Subject: subj
        To: alice@example.org
        Message-ID: <hsabaeni@example.org>
        Content-Type: text/plain; charset=utf-8

        message in Sent
    """.format(
            ac1.get_config("configured_addr")
        ),
    )
    ac1.direct_imap.append(
        "Spam",
        """
        From: unknown.address@junk.org
        Subject: subj
        To: {}
        Message-ID: <spam.message@junk.org>
        Content-Type: text/plain; charset=utf-8

        Unknown message in Spam
    """.format(
            ac1.get_config("configured_addr")
        ),
    )
    ac1.direct_imap.append(
        "Junk",
        """
        From: unknown.address@junk.org
        Subject: subj
        To: {}
        Message-ID: <spam.message@junk.org>
        Content-Type: text/plain; charset=utf-8

        Unknown message in Junk
    """.format(
            ac1.get_config("configured_addr")
        ),
    )

    ac1.set_config("scan_all_folders_debounce_secs", "0")
    lp.sec("All prepared, now let DC find the message")
    ac1.start_io()

    msg = ac1._evtracker.wait_next_messages_changed()

    # Wait until each folder was scanned, this is necessary for this test to test what it should test:
    ac1._evtracker.wait_idle_inbox_ready()

    assert msg.text == "subj – message in Sent"
    assert len(msg.chat.get_messages()) == 1

    assert not any("unknown.address" in c.get_name() for c in ac1.get_chats())
    ac1.direct_imap.select_folder("Spam")
    assert ac1.direct_imap.get_uid_by_message_id("spam.message@junk.org")

    ac1.stop_io()
    lp.sec("'Send out' the draft, i.e. move it to the Sent folder, and wait for DC to display it this time")
    ac1.direct_imap.select_folder("Drafts")
    uid = ac1.direct_imap.get_uid_by_message_id("aepiors@example.org")
    ac1.direct_imap.conn.move(uid, "Sent")

    ac1.start_io()
    msg2 = ac1._evtracker.wait_next_messages_changed()

    assert msg2.text == "subj – message in Drafts that is moved to Sent later"
    assert len(msg.chat.get_messages()) == 2


def test_no_old_msg_is_fresh(acfactory, lp):
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account()
    ac1_clone = acfactory.new_online_configuring_account(cloned_from=ac1)
    acfactory.bring_accounts_online()

    ac1.set_config("e2ee_enabled", "0")
    ac1_clone.set_config("e2ee_enabled", "0")
    ac2.set_config("e2ee_enabled", "0")

    ac1_clone.set_config("bcc_self", "1")

    ac1.create_chat(ac2)
    ac1_clone.create_chat(ac2)

    ac1.get_device_chat().mark_noticed()

    lp.sec("Send a first message from ac2 to ac1 and check that it's 'fresh'")
    first_msg_id = ac2.create_chat(ac1).send_text("Hi")
    ac1._evtracker.wait_next_incoming_message()
    assert ac1.create_chat(ac2).count_fresh_messages() == 1
    assert len(list(ac1.get_fresh_messages())) == 1

    lp.sec("Send a message from ac1_clone to ac2 and check that ac1 marks the first message as 'noticed'")
    ac1_clone.create_chat(ac2).send_text("Hi back")
    ev = ac1._evtracker.get_matching("DC_EVENT_MSGS_NOTICED")

    assert ev.data1 == first_msg_id.chat.id
    assert ac1.create_chat(ac2).count_fresh_messages() == 0
    assert len(list(ac1.get_fresh_messages())) == 0


def test_prefer_encrypt(acfactory, lp):
    """Test quorum rule for encryption preference in 1:1 and group chat."""
    ac1, ac2, ac3 = acfactory.get_online_accounts(3)
    ac1.set_config("e2ee_enabled", "0")
    ac2.set_config("e2ee_enabled", "1")
    ac3.set_config("e2ee_enabled", "0")

    # Make sure we do not send a copy to ourselves. This is to
    # test that we count own preference even when we are not in
    # the recipient list.
    ac1.set_config("bcc_self", "0")
    ac2.set_config("bcc_self", "0")
    ac3.set_config("bcc_self", "0")

    acfactory.introduce_each_other([ac1, ac2, ac3])

    lp.sec("ac1: sending message to ac2")
    chat1 = ac1.create_chat(ac2)
    msg1 = chat1.send_text("message1")
    assert not msg1.is_encrypted()
    ac2._evtracker.wait_next_incoming_message()

    lp.sec("ac2: sending message to ac1")
    chat2 = ac2.create_chat(ac1)
    msg2 = chat2.send_text("message2")
    assert not msg2.is_encrypted()
    ac1._evtracker.wait_next_incoming_message()

    lp.sec("ac1: sending message to group chat with ac2 and ac3")
    group = ac1.create_group_chat("hello")
    group.add_contact(ac2)
    group.add_contact(ac3)
    msg3 = group.send_text("message3")
    assert not msg3.is_encrypted()
    ac2._evtracker.wait_next_incoming_message()
    ac3._evtracker.wait_next_incoming_message()

    lp.sec("ac3: start preferring encryption and inform ac1")
    ac3.set_config("e2ee_enabled", "1")
    chat3 = ac3.create_chat(ac1)
    msg4 = chat3.send_text("message4")
    # ac1 still does not prefer encryption
    assert not msg4.is_encrypted()
    ac1._evtracker.wait_next_incoming_message()

    lp.sec("ac1: sending another message to group chat with ac2 and ac3")
    msg5 = group.send_text("message5")
    # Majority prefers encryption now
    assert msg5.is_encrypted()


def test_bot(acfactory, lp):
    """Test that bot messages can be identified as such"""
    ac1, ac2 = acfactory.get_online_accounts(2)
    ac1.set_config("bot", "0")
    ac2.set_config("bot", "1")

    lp.sec("ac1: create chat with ac2")
    chat = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("sending a message from ac1 to ac2")
    text1 = "hello"
    chat.send_text(text1)

    lp.sec("wait for ac2 to receive a message")
    msg_in = ac2._evtracker.wait_next_incoming_message()
    assert msg_in.text == text1
    assert not msg_in.is_bot()

    lp.sec("sending a message from ac2 to ac1")
    text2 = "reply"
    msg_in.chat.send_text(text2)

    lp.sec("wait for ac1 to receive a message")
    msg_in = ac1._evtracker.wait_next_incoming_message()
    assert msg_in.text == text2
    assert msg_in.is_bot()


def test_quote_encrypted(acfactory, lp):
    """Test that replies to encrypted messages with quotes are encrypted."""
    ac1, ac2 = acfactory.get_online_accounts(2)

    lp.sec("ac1: create chat with ac2")
    chat = ac1.create_chat(ac2)

    lp.sec("sending text message from ac1 to ac2")
    msg1 = chat.send_text("message1")
    assert not msg1.is_encrypted()

    lp.sec("wait for ac2 to receive message")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "message1"
    assert not msg2.is_encrypted()

    lp.sec("create new chat with contact and send back (encrypted) message")
    msg2.create_chat().send_text("message-back")

    lp.sec("wait for ac1 to receive message")
    msg3 = ac1._evtracker.wait_next_incoming_message()
    assert msg3.text == "message-back"
    assert msg3.is_encrypted()

    lp.sec("ac1: e2ee_enabled=0 and see if reply is encrypted")
    print("ac1: e2ee_enabled={}".format(ac1.get_config("e2ee_enabled")))
    print("ac2: e2ee_enabled={}".format(ac2.get_config("e2ee_enabled")))
    ac1.set_config("e2ee_enabled", "0")

    for quoted_msg in msg1, msg3:
        # Save the draft with a quote.
        # It should be encrypted if quoted message is encrypted.
        msg_draft = Message.new_empty(ac1, "text")
        msg_draft.set_text("message reply")
        msg_draft.quote = quoted_msg
        chat.set_draft(msg_draft)

        # Get the draft, prepare and send it.
        msg_draft = chat.get_draft()
        msg_out = chat.prepare_message(msg_draft)
        chat.send_prepared(msg_out)

        chat.set_draft(None)
        assert chat.get_draft() is None

        msg_in = ac2._evtracker.wait_next_incoming_message()
        assert msg_in.text == "message reply"
        assert msg_in.quoted_text == quoted_msg.text
        assert msg_in.is_encrypted() == quoted_msg.is_encrypted()


def test_quote_attachment(tmpdir, acfactory, lp):
    """Test that replies with an attachment and a quote are received correctly."""
    ac1, ac2 = acfactory.get_online_accounts(2)

    lp.sec("ac1 creates chat with ac2")
    chat1 = ac1.create_chat(ac2)

    lp.sec("ac1 sends text message to ac2")
    chat1.send_text("hi")

    lp.sec("ac2 receives contact request from ac1")
    received_message = ac2._evtracker.wait_next_incoming_message()
    assert received_message.text == "hi"

    basename = "attachment.txt"
    p = os.path.join(tmpdir.strpath, basename)
    with open(p, "w") as f:
        f.write("data to send")

    lp.sec("ac2 sends a reply to ac1")
    chat2 = received_message.create_chat()
    reply = Message.new_empty(ac2, "file")
    reply.set_text("message reply")
    reply.set_file(p)
    reply.quote = received_message
    chat2.send_msg(reply)

    lp.sec("ac1 receives a reply from ac2")
    received_reply = ac1._evtracker.wait_next_incoming_message()
    assert received_reply.text == "message reply"
    assert received_reply.quoted_text == received_message.text
    assert open(received_reply.filename).read() == "data to send"


def test_saved_mime_on_received_message(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)

    lp.sec("configure ac2 to save mime headers, create ac1/ac2 chat")
    ac2.set_config("save_mime_headers", "1")
    chat = ac1.create_chat(ac2)

    lp.sec("sending text message from ac1 to ac2")
    msg_out = chat.send_text("message1")
    ac1._evtracker.wait_msg_delivered(msg_out)
    assert msg_out.get_mime_headers() is None

    lp.sec("wait for ac2 to receive message")
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG")
    in_id = ev.data2
    mime = ac2.get_message_by_id(in_id).get_mime_headers()
    assert mime.get_all("From")
    assert mime.get_all("Received")


def test_send_mark_seen_clean_incoming_events(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = acfactory.get_accepted_chat(ac1, ac2)

    message_queue = queue.Queue()

    class InPlugin:
        @account_hookimpl
        def ac_incoming_message(self, message):
            message_queue.put(message)

    ac1.add_account_plugin(InPlugin())

    lp.sec("sending one message from ac1 to ac2")
    chat.send_text("hello")

    lp.sec("ac2: waiting to receive")
    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.text == "hello"

    lp.sec("ac2: mark seen {}".format(msg))
    msg.mark_seen()

    for ev in ac1._evtracker.iter_events():
        if ev.name == "DC_EVENT_INCOMING_MSG":
            pytest.fail("MDN arrived as regular incoming message")
        elif ev.name == "DC_EVENT_MSG_READ":
            break


def test_send_and_receive_image(acfactory, lp, data):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat = ac1.create_chat(ac2)

    message_queue = queue.Queue()

    class InPlugin:
        @account_hookimpl
        def ac_incoming_message(self, message):
            message_queue.put(message)

    delivered = queue.Queue()
    out = queue.Queue()

    class OutPlugin:
        @account_hookimpl
        def ac_message_delivered(self, message):
            delivered.put(message)

        @account_hookimpl
        def ac_outgoing_message(self, message):
            out.put(message)

    ac1.add_account_plugin(OutPlugin())
    ac2.add_account_plugin(InPlugin())

    lp.sec("sending image message from ac1 to ac2")
    path = data.get_path("d.png")
    msg_out = chat.send_image(path)
    ac1._evtracker.wait_msg_delivered(msg_out)
    m = out.get()
    assert m == msg_out
    m = delivered.get()
    assert m == msg_out

    lp.sec("wait for ac2 to receive message")
    ev = ac2._evtracker.get_matching("DC_EVENT_MSGS_CHANGED|DC_EVENT_INCOMING_MSG")
    assert ev.data2 == msg_out.id
    msg_in = ac2.get_message_by_id(msg_out.id)
    assert msg_in.is_image()
    assert os.path.exists(msg_in.filename)
    assert os.stat(msg_in.filename).st_size == os.stat(path).st_size
    m = message_queue.get()
    assert m == msg_in


def test_import_export_online_all(acfactory, tmpdir, data, lp):
    (ac1,) = acfactory.get_online_accounts(1)

    lp.sec("create some chat content")
    chat1 = ac1.create_contact("some1@example.org", name="some1").create_chat()
    chat1.send_text("msg1")
    assert len(ac1.get_contacts(query="some1")) == 1

    original_image_path = data.get_path("d.png")
    chat1.send_image(original_image_path)

    # Add another 100KB file that ensures that the progress is smooth enough
    path = tmpdir.join("attachment.txt")
    with open(path, "w") as file:
        file.truncate(100000)
    chat1.send_file(path.strpath)

    def assert_account_is_proper(ac):
        contacts = ac.get_contacts(query="some1")
        assert len(contacts) == 1
        contact2 = contacts[0]
        assert contact2.addr == "some1@example.org"
        chat2 = contact2.create_chat()
        messages = chat2.get_messages()
        assert len(messages) == 3
        assert messages[0].text == "msg1"
        assert messages[1].filemime == "image/png"
        assert os.stat(messages[1].filename).st_size == os.stat(original_image_path).st_size
        ac.set_config("displayname", "new displayname")
        assert ac.get_config("displayname") == "new displayname"

    assert_account_is_proper(ac1)

    backupdir = tmpdir.mkdir("backup")

    lp.sec("export all to {}".format(backupdir))
    with ac1.temp_plugin(ImexTracker()) as imex_tracker:

        ac1.stop_io()
        ac1.imex(backupdir.strpath, const.DC_IMEX_EXPORT_BACKUP)

        # check progress events for export
        assert imex_tracker.wait_progress(1, progress_upper_limit=249)
        assert imex_tracker.wait_progress(250, progress_upper_limit=499)
        assert imex_tracker.wait_progress(500, progress_upper_limit=749)
        assert imex_tracker.wait_progress(750, progress_upper_limit=999)

        paths = imex_tracker.wait_finish()
        assert len(paths) == 1
        path = paths[0]
        assert os.path.exists(path)
        ac1.start_io()

    lp.sec("get fresh empty account")
    ac2 = acfactory.get_unconfigured_account()

    lp.sec("get latest backup file")
    path2 = ac2.get_latest_backupfile(backupdir.strpath)
    assert path2 == path

    lp.sec("import backup and check it's proper")
    with ac2.temp_plugin(ImexTracker()) as imex_tracker:
        ac2.import_all(path)

        # check progress events for import
        assert imex_tracker.wait_progress(1, progress_upper_limit=249)
        assert imex_tracker.wait_progress(500, progress_upper_limit=749)
        assert imex_tracker.wait_progress(750, progress_upper_limit=999)
        assert imex_tracker.wait_progress(1000)

    assert_account_is_proper(ac1)
    assert_account_is_proper(ac2)

    lp.sec("Second-time export all to {}".format(backupdir))
    ac1.stop_io()
    path2 = ac1.export_all(backupdir.strpath)
    assert os.path.exists(path2)
    assert path2 != path
    assert ac2.get_latest_backupfile(backupdir.strpath) == path2


def test_ac_setup_message(acfactory, lp):
    # note that the receiving account needs to be configured and running
    # before ther setup message is send. DC does not read old messages
    # as of Jul2019
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account(cloned_from=ac1)
    acfactory.bring_accounts_online()

    lp.sec("trigger ac setup message and return setupcode")
    assert ac1.get_info()["fingerprint"] != ac2.get_info()["fingerprint"]
    setup_code = ac1.initiate_key_transfer()
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG|DC_EVENT_MSGS_CHANGED")
    msg = ac2.get_message_by_id(ev.data2)
    assert msg.is_setup_message()
    assert msg.get_setupcodebegin() == setup_code[:2]
    lp.sec("try a bad setup code")
    with pytest.raises(ValueError):
        msg.continue_key_transfer(str(reversed(setup_code)))
    lp.sec("try a good setup code")
    print("*************** Incoming ASM File at: ", msg.filename)
    print("*************** Setup Code: ", setup_code)
    msg.continue_key_transfer(setup_code)
    assert ac1.get_info()["fingerprint"] == ac2.get_info()["fingerprint"]


def test_ac_setup_message_twice(acfactory, lp):
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account(cloned_from=ac1)
    acfactory.bring_accounts_online()

    lp.sec("trigger ac setup message but ignore")
    assert ac1.get_info()["fingerprint"] != ac2.get_info()["fingerprint"]
    ac1.initiate_key_transfer()
    ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG|DC_EVENT_MSGS_CHANGED")

    lp.sec("trigger second ac setup message, wait for receive ")
    setup_code2 = ac1.initiate_key_transfer()
    ev = ac2._evtracker.get_matching("DC_EVENT_INCOMING_MSG|DC_EVENT_MSGS_CHANGED")
    msg = ac2.get_message_by_id(ev.data2)
    assert msg.is_setup_message()
    assert msg.get_setupcodebegin() == setup_code2[:2]

    lp.sec("process second setup message")
    msg.continue_key_transfer(setup_code2)
    assert ac1.get_info()["fingerprint"] == ac2.get_info()["fingerprint"]


def test_qr_setup_contact(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    lp.sec("ac1: create QR code and let ac2 scan it, starting the securejoin")
    qr = ac1.get_setup_contact_qr()

    lp.sec("ac2: start QR-code based setup contact protocol")
    ch = ac2.qr_setup_contact(qr)
    assert ch.id >= 10
    ac1._evtracker.wait_securejoin_inviter_progress(1000)


def test_qr_join_chat(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    lp.sec("ac1: create QR code and let ac2 scan it, starting the securejoin")
    chat = ac1.create_group_chat("hello")
    qr = chat.get_join_qr()
    lp.sec("ac2: start QR-code based join-group protocol")
    ch = ac2.qr_join_chat(qr)
    lp.sec("ac2: qr_join_chat() returned")
    assert ch.id >= 10
    # check that at least some of the handshake messages are deleted
    ac1._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_DELETED")
    ac2._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_DELETED")
    ac1._evtracker.wait_securejoin_inviter_progress(1000)


def test_set_get_contact_avatar(acfactory, data, lp):
    lp.sec("configuring ac1 and ac2")
    ac1, ac2 = acfactory.get_online_accounts(2)

    lp.sec("set ac1 and ac2 profile images")
    p = data.get_path("d.png")
    ac1.set_avatar(p)
    ac2.set_avatar(p)

    lp.sec("ac1: send message to ac2")
    ac1.create_chat(ac2).send_text("with avatar!")

    lp.sec("ac2: wait for receiving message and avatar from ac1")
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.chat.is_contact_request()
    received_path = msg2.get_sender_contact().get_profile_image()
    assert open(received_path, "rb").read() == open(p, "rb").read()

    lp.sec("ac2: send back message")
    msg3 = msg2.create_chat().send_text("yes, i received your avatar -- how do you like mine?")
    assert msg3.is_encrypted()

    lp.sec("ac1: wait for receiving message and avatar from ac2")
    msg4 = ac1._evtracker.wait_next_incoming_message()
    received_path = msg4.get_sender_contact().get_profile_image()
    assert received_path is not None, "did not get avatar through encrypted message"
    assert open(received_path, "rb").read() == open(p, "rb").read()

    ac2._evtracker.consume_events()
    ac1._evtracker.consume_events()

    lp.sec("ac1: delete profile image from chat, and send message to ac2")
    ac1.set_avatar(None)
    msg5 = ac1.create_chat(ac2).send_text("removing my avatar")
    assert msg5.is_encrypted()

    lp.sec("ac2: wait for message along with avatar deletion of ac1")
    msg6 = ac2._evtracker.wait_next_incoming_message()
    assert msg6.get_sender_contact().get_profile_image() is None


def test_add_remove_member_remote_events(acfactory, lp):
    ac1, ac2, ac3 = acfactory.get_online_accounts(3)
    ac1_addr = ac1.get_config("addr")
    ac3_addr = ac3.get_config("addr")
    # activate local plugin for ac2
    in_list = queue.Queue()

    class EventHolder:
        def __init__(self, **kwargs):
            self.__dict__.update(kwargs)

    class InPlugin:
        @account_hookimpl
        def ac_incoming_message(self, message):
            # we immediately accept the sender because
            # otherwise we won't see member_added contacts
            message.create_chat()

        @account_hookimpl
        def ac_chat_modified(self, chat):
            in_list.put(EventHolder(action="chat-modified", chat=chat))

        @account_hookimpl
        def ac_member_added(self, chat, contact, message):
            in_list.put(EventHolder(action="added", chat=chat, contact=contact, message=message))

        @account_hookimpl
        def ac_member_removed(self, chat, contact, message):
            in_list.put(EventHolder(action="removed", chat=chat, contact=contact, message=message))

    ac2.add_account_plugin(InPlugin())

    lp.sec("ac1: create group chat with ac2")
    chat = ac1.create_group_chat("hello", contacts=[ac2])

    lp.sec("ac1: send a message to group chat to promote the group")
    chat.send_text("afterwards promoted")
    ev = in_list.get()
    assert ev.action == "chat-modified"
    assert chat.is_promoted()
    assert sorted(x.addr for x in chat.get_contacts()) == sorted(x.addr for x in ev.chat.get_contacts())

    lp.sec("ac1: add address2")
    # note that if the above create_chat() would not
    # happen we would not receive a proper member_added event
    contact2 = chat.add_contact(ac3_addr)
    ev = in_list.get()
    assert ev.action == "chat-modified"
    ev = in_list.get()
    assert ev.action == "chat-modified"
    ev = in_list.get()
    assert ev.action == "added"
    assert ev.message.get_sender_contact().addr == ac1_addr
    assert ev.contact.addr == ac3_addr

    lp.sec("ac1: remove address2")
    chat.remove_contact(contact2)
    ev = in_list.get()
    assert ev.action == "chat-modified"
    ev = in_list.get()
    assert ev.action == "removed"
    assert ev.contact.addr == contact2.addr
    assert ev.message.get_sender_contact().addr == ac1_addr

    lp.sec("ac1: remove ac2 contact from chat")
    chat.remove_contact(ac2)
    ev = in_list.get()
    assert ev.action == "chat-modified"
    ev = in_list.get()
    assert ev.action == "removed"
    assert ev.message.get_sender_contact().addr == ac1_addr


def test_system_group_msg_from_blocked_user(acfactory, lp):
    """
    Tests that a blocked user removes you from a group.
    The message has to be fetched even though the user is blocked
    to avoid inconsistent group state.
    Also tests blocking in general.
    """
    lp.sec("Create a group chat with ac1 and ac2")
    (ac1, ac2) = acfactory.get_online_accounts(2)
    acfactory.introduce_each_other((ac1, ac2))
    chat_on_ac1 = ac1.create_group_chat("title", contacts=[ac2])
    chat_on_ac1.send_text("First group message")
    chat_on_ac2 = ac2._evtracker.wait_next_incoming_message().chat

    lp.sec("ac1 blocks ac2")
    contact = ac1.create_contact(ac2)
    contact.block()
    assert contact.is_blocked()
    ev = ac1._evtracker.get_matching("DC_EVENT_CONTACTS_CHANGED")
    assert ev.data1 == contact.id

    lp.sec("ac2 sends a message to ac1 that does not arrive because it is blocked")
    ac2.create_chat(ac1).send_text("This will not arrive!")

    lp.sec("ac2 sends a group message to ac1 that arrives")
    # Groups would be hardly usable otherwise: If you have blocked some
    # users, they write messages and you only see replies to them without context
    chat_on_ac2.send_text("This will arrive")
    msg = ac1._evtracker.wait_next_incoming_message()
    assert msg.text == "This will arrive"
    message_texts = [m.text for m in chat_on_ac1.get_messages()]
    assert len(message_texts) == 2
    assert "First group message" in message_texts
    assert "This will arrive" in message_texts

    lp.sec("ac2 removes ac1 from their group")
    assert ac1.get_self_contact() in chat_on_ac1.get_contacts()
    assert contact.is_blocked()
    chat_on_ac2.remove_contact(ac1)
    ac1._evtracker.get_matching("DC_EVENT_CHAT_MODIFIED")
    assert not ac1.get_self_contact() in chat_on_ac1.get_contacts()


def test_set_get_group_image(acfactory, data, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)

    lp.sec("create unpromoted group chat")
    chat = ac1.create_group_chat("hello")
    p = data.get_path("d.png")

    lp.sec("ac1: set profile image on unpromoted chat")
    chat.set_profile_image(p)
    ac1._evtracker.get_matching("DC_EVENT_CHAT_MODIFIED")
    assert not chat.is_promoted()

    lp.sec("ac1: send text to promote chat (XXX without contact added)")
    # XXX first promote the chat before adding contact
    # because DC does not send out profile images for unpromoted chats
    # otherwise
    chat.send_text("ac1: initial message to promote chat (workaround)")
    assert chat.is_promoted()
    assert chat.get_profile_image()

    lp.sec("ac2: check that initial message arrived")
    ac2.create_contact(ac1).create_chat()
    ac2._evtracker.get_matching("DC_EVENT_MSGS_CHANGED")

    lp.sec("ac1: add ac2 to promoted group chat")
    chat.add_contact(ac2)  # sends one message

    lp.sec("ac1: send a first message to ac2")
    chat.send_text("hi")  # sends another message
    assert chat.is_promoted()

    lp.sec("ac2: wait for receiving message from ac1")
    msg1 = ac2._evtracker.wait_next_incoming_message()
    assert msg1.is_system_message()  # Member added
    msg2 = ac2._evtracker.wait_next_incoming_message()
    assert msg2.text == "hi"
    assert msg1.chat.id == msg2.chat.id

    lp.sec("ac2: see if chat now has got the profile image")
    p2 = msg1.chat.get_profile_image()
    assert p2 is not None
    assert open(p2, "rb").read() == open(p, "rb").read()

    ac2._evtracker.consume_events()
    ac1._evtracker.consume_events()

    lp.sec("ac2: delete profile image from chat")
    msg1.chat.remove_profile_image()
    msg_back = ac1._evtracker.wait_next_incoming_message()
    assert msg_back.text == "Group image deleted by {}.".format(ac2.get_config("addr"))
    assert msg_back.is_system_message()
    assert msg_back.chat == chat
    assert chat.get_profile_image() is None


def test_connectivity(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    ac1.set_config("scan_all_folders_debounce_secs", "0")

    ac1._evtracker.wait_for_connectivity(const.DC_CONNECTIVITY_CONNECTED)

    lp.sec("Test stop_io() and start_io()")
    ac1.stop_io()
    ac1._evtracker.wait_for_connectivity(const.DC_CONNECTIVITY_NOT_CONNECTED)

    ac1.start_io()
    ac1._evtracker.wait_for_connectivity(const.DC_CONNECTIVITY_CONNECTING)
    ac1._evtracker.wait_for_connectivity_change(const.DC_CONNECTIVITY_CONNECTING, const.DC_CONNECTIVITY_CONNECTED)

    lp.sec(
        "Test that after calling start_io(), maybe_network() and waiting for `all_work_done()`, "
        + "all messages are fetched"
    )

    ac1.direct_imap.select_config_folder("inbox")
    with ac1.direct_imap.idle() as idle1:
        ac2.create_chat(ac1).send_text("Hi")
        idle1.wait_for_new_message()
    ac1.maybe_network()
    ac1._evtracker.wait_for_all_work_done()
    msgs = ac1.create_chat(ac2).get_messages()
    assert len(msgs) == 1
    assert msgs[0].text == "Hi"

    lp.sec("Test that the connectivity changes to WORKING while new messages are fetched")

    ac2.create_chat(ac1).send_text("Hi 2")

    ac1._evtracker.wait_for_connectivity_change(const.DC_CONNECTIVITY_CONNECTED, const.DC_CONNECTIVITY_WORKING)
    ac1._evtracker.wait_for_connectivity_change(const.DC_CONNECTIVITY_WORKING, const.DC_CONNECTIVITY_CONNECTED)

    msgs = ac1.create_chat(ac2).get_messages()
    assert len(msgs) == 2
    assert msgs[1].text == "Hi 2"

    lp.sec("Test that the connectivity doesn't flicker to WORKING if there are no new messages")

    ac1.maybe_network()
    while 1:
        assert ac1.get_connectivity() == const.DC_CONNECTIVITY_CONNECTED
        if ac1.all_work_done():
            break
        ac1._evtracker.get_matching("DC_EVENT_CONNECTIVITY_CHANGED")

    lp.sec("Test that the connectivity doesn't flicker to WORKING if the sender of the message is blocked")
    ac1.create_contact(ac2).block()

    ac1.direct_imap.select_config_folder("inbox")
    with ac1.direct_imap.idle() as idle1:
        ac2.create_chat(ac1).send_text("Hi")
        idle1.wait_for_new_message()
    ac1.maybe_network()

    while 1:
        assert ac1.get_connectivity() == const.DC_CONNECTIVITY_CONNECTED
        if ac1.all_work_done():
            break
        ac1._evtracker.get_matching("DC_EVENT_CONNECTIVITY_CHANGED")

    lp.sec("Test that the connectivity is NOT_CONNECTED if the password is wrong")

    ac1.set_config("configured_mail_pw", "abc")
    ac1.stop_io()
    ac1._evtracker.wait_for_connectivity(const.DC_CONNECTIVITY_NOT_CONNECTED)
    ac1.start_io()
    ac1._evtracker.wait_for_connectivity(const.DC_CONNECTIVITY_CONNECTING)
    ac1._evtracker.wait_for_connectivity(const.DC_CONNECTIVITY_NOT_CONNECTED)


def test_fetch_deleted_msg(acfactory, lp):
    """This is a regression test: Messages with \\Deleted flag were downloaded again and again,
    hundreds of times, because uid_next was not updated.

    See https://github.com/deltachat/deltachat-core-rust/issues/2429.
    """
    (ac1,) = acfactory.get_online_accounts(1)
    ac1.stop_io()

    ac1.direct_imap.append(
        "INBOX",
        """
        From: alice <alice@example.org>
        Subject: subj
        To: bob@example.com
        Chat-Version: 1.0
        Message-ID: <aepiors@example.org>
        Content-Type: text/plain; charset=utf-8

        Deleted message
    """,
    )
    ac1.direct_imap.delete("1:*", expunge=False)
    ac1.start_io()

    for ev in ac1._evtracker.iter_events():
        if ev.name == "DC_EVENT_MSGS_CHANGED":
            pytest.fail("A deleted message was shown to the user")

        if ev.name == "DC_EVENT_INFO" and "1 mails read from" in ev.data2:
            break

    # The message was downloaded once, now check that it's not downloaded again

    for ev in ac1._evtracker.iter_events():
        if ev.name == "DC_EVENT_INFO" and "1 mails read from" in ev.data2:
            pytest.fail("The same email was read twice")

        if ev.name == "DC_EVENT_MSGS_CHANGED":
            pytest.fail("A deleted message was shown to the user")

        if ev.name == "DC_EVENT_INFO" and "INBOX: Idle entering wait-on-remote state" in ev.data2:
            break  # DC is done with reading messages


def test_send_receive_locations(acfactory, lp):
    now = datetime.now(timezone.utc)
    ac1, ac2 = acfactory.get_online_accounts(2)

    lp.sec("ac1: create chat with ac2")
    chat1 = ac1.create_chat(ac2)
    chat2 = ac2.create_chat(ac1)

    assert not chat1.is_sending_locations()
    with pytest.raises(ValueError):
        ac1.set_location(latitude=0.0, longitude=10.0)

    ac1._evtracker.consume_events()
    ac2._evtracker.consume_events()

    lp.sec("ac1: enable location sending in chat")
    chat1.enable_sending_locations(seconds=100)
    assert chat1.is_sending_locations()
    ac1._evtracker.get_matching("DC_EVENT_SMTP_MESSAGE_SENT")

    ac1.set_location(latitude=2.0, longitude=3.0, accuracy=0.5)
    ac1._evtracker.get_matching("DC_EVENT_LOCATION_CHANGED")
    chat1.send_text("🍞")
    ac1._evtracker.get_matching("DC_EVENT_SMTP_MESSAGE_SENT")

    lp.sec("ac2: wait for incoming location message")

    # currently core emits location changed before event_incoming message
    ac2._evtracker.get_matching("DC_EVENT_LOCATION_CHANGED")

    locations = chat2.get_locations()
    assert len(locations) == 1
    assert locations[0].latitude == 2.0
    assert locations[0].longitude == 3.0
    assert locations[0].accuracy == 0.5
    assert locations[0].timestamp > now
    assert locations[0].marker == "🍞"

    contact = ac2.create_contact(ac1)
    locations2 = chat2.get_locations(contact=contact)
    assert len(locations2) == 1
    assert locations2 == locations

    contact = ac2.create_contact("nonexisting@example.org")
    locations3 = chat2.get_locations(contact=contact)
    assert not locations3


def test_immediate_autodelete(acfactory, lp):
    ac1 = acfactory.new_online_configuring_account()
    ac2 = acfactory.new_online_configuring_account()

    # "1" means delete immediately, while "0" means do not delete
    ac2.set_config("delete_server_after", "1")

    acfactory.bring_accounts_online()

    lp.sec("ac1: create chat with ac2")
    chat1 = ac1.create_chat(ac2)
    ac2.create_chat(ac1)

    lp.sec("ac1: send message to ac2")
    sent_msg = chat1.send_text("hello")

    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.text == "hello"

    lp.sec("ac2: wait for close/expunge on autodelete")
    ac2._evtracker.get_info_contains("close/expunge succeeded")

    lp.sec("ac2: check that message was autodeleted on server")
    assert len(ac2.direct_imap.get_all_messages()) == 0

    lp.sec("ac2: Mark deleted message as seen and check that read receipt arrives")
    msg.mark_seen()
    ev = ac1._evtracker.get_matching("DC_EVENT_MSG_READ")
    assert ev.data1 == chat1.id
    assert ev.data2 == sent_msg.id


def test_delete_multiple_messages(acfactory, lp):
    ac1, ac2 = acfactory.get_online_accounts(2)
    chat12 = acfactory.get_accepted_chat(ac1, ac2)

    lp.sec("ac1: sending seven messages")
    texts = ["first", "second", "third", "fourth", "fifth", "sixth", "seventh"]
    for text in texts:
        chat12.send_text(text)

    lp.sec("ac2: waiting for all messages on the other side")
    to_delete = []
    for text in texts:
        msg = ac2._evtracker.wait_next_incoming_message()
        assert msg.text in texts
        if text != "third":
            to_delete.append(msg)

    lp.sec("ac2: deleting all messages except third")
    assert len(to_delete) == len(texts) - 1
    ac2.delete_messages(to_delete)
    ac2._evtracker.get_matching("DC_EVENT_IMAP_MESSAGE_DELETED")

    ac2._evtracker.get_info_contains("close/expunge succeeded")

    lp.sec("ac2: test that only one message is left")
    ac2.direct_imap.select_config_folder("inbox")
    assert len(ac2.direct_imap.get_all_messages()) == 1


def test_configure_error_msgs_wrong_pw(acfactory):
    configdict = acfactory.get_next_liveconfig()
    ac1 = acfactory.get_unconfigured_account()
    ac1.update_config(configdict)
    ac1.set_config("mail_pw", "abc")  # Wrong mail pw
    ac1.configure()
    while True:
        ev = ac1._evtracker.get_matching("DC_EVENT_CONFIGURE_PROGRESS")
        if ev.data1 == 0:
            break
    # Password is wrong so it definitely has to say something about "password"
    assert "password" in ev.data2


def test_configure_error_msgs_invalid_server(acfactory):
    ac2 = acfactory.get_unconfigured_account()
    ac2.set_config("addr", "abc@def.invalid")  # mail server can't be reached
    ac2.set_config("mail_pw", "123")
    ac2.configure()
    while True:
        ev = ac2._evtracker.get_matching("DC_EVENT_CONFIGURE_PROGRESS")
        if ev.data1 == 0:
            break
    # Can't connect so it probably should say something about "internet"
    # again, should not repeat itself
    # If this fails then probably `e.msg.to_lowercase().contains("could not resolve")`
    # in configure.rs returned false because the error message was changed
    # (i.e. did not contain "could not resolve" anymore)
    assert (ev.data2.count("internet") + ev.data2.count("network")) == 1
    # Should mention that it can't connect:
    assert ev.data2.count("connect") == 1
    # The users do not know what "configuration" is
    assert "configuration" not in ev.data2.lower()


def test_name_changes(acfactory):
    ac1, ac2 = acfactory.get_online_accounts(2)
    ac1.set_config("displayname", "Account 1")

    # Similar to acfactory.get_accepted_chat, but without setting the contact name.
    ac2.create_contact(ac1.get_config("addr")).create_chat()
    chat12 = ac1.create_contact(ac2.get_config("addr")).create_chat()
    contact = None

    def update_name():
        """Send a message from ac1 to ac2 to update the name"""
        nonlocal contact
        chat12.send_text("Hello")
        msg = ac2._evtracker.wait_next_incoming_message()
        contact = msg.get_sender_contact()
        return contact.name

    assert update_name() == "Account 1"

    ac1.set_config("displayname", "Account 1 revision 2")
    assert update_name() == "Account 1 revision 2"

    # Explicitly rename contact on ac2 to "Renamed"
    ac2.create_contact(contact, name="Renamed")
    assert contact.name == "Renamed"
    ev = ac2._evtracker.get_matching("DC_EVENT_CONTACTS_CHANGED")
    assert ev.data1 == contact.id

    # ac1 also renames itself into "Renamed"
    assert update_name() == "Renamed"
    ac1.set_config("displayname", "Renamed")
    assert update_name() == "Renamed"

    # Contact name was set to "Renamed" explicitly before,
    # so it should not be changed.
    ac1.set_config("displayname", "Renamed again")
    updated_name = update_name()
    assert updated_name == "Renamed"


def test_status(acfactory):
    """Test that status is transferred over the network."""
    ac1, ac2 = acfactory.get_online_accounts(2)

    chat12 = acfactory.get_accepted_chat(ac1, ac2)
    ac1.set_config("selfstatus", "New status")
    chat12.send_text("hi")
    msg_received = ac2._evtracker.wait_next_incoming_message()
    assert msg_received.text == "hi"
    assert msg_received.get_sender_contact().status == "New status"

    # Send a reply from ac2 to ac1 so ac1 can send a read receipt.
    reply_msg = msg_received.chat.send_text("reply")
    reply_msg_received = ac1._evtracker.wait_next_incoming_message()
    assert reply_msg_received.text == "reply"

    # Send read receipt from ac1 to ac2.
    # It does not contain the signature.
    ac1.mark_seen_messages([reply_msg_received])
    ev = ac2._evtracker.get_matching("DC_EVENT_MSG_READ")
    assert ev.data1 == reply_msg.chat.id
    assert ev.data2 == reply_msg.id
    assert reply_msg.is_out_mdn_received()

    # Test that the status is not cleared as a result of receiving a read receipt.
    assert msg_received.get_sender_contact().status == "New status"

    ac1.set_config("selfstatus", "")
    chat12.send_text("hello")
    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.text == "hello"
    assert msg.get_sender_contact().status == ""


def test_group_quote(acfactory, lp):
    """Test quoting in a group with a new member who have not seen the quoted message."""
    ac1, ac2, ac3 = accounts = acfactory.get_online_accounts(3)
    acfactory.introduce_each_other(accounts)
    chat = ac1.create_group_chat(name="quote group")
    chat.add_contact(ac2)

    lp.sec("ac1: sending message")
    out_msg = chat.send_text("hello")

    lp.sec("ac2: receiving message")
    msg = ac2._evtracker.wait_next_incoming_message()
    assert msg.text == "hello"

    chat.add_contact(ac3)
    ac2._evtracker.wait_next_incoming_message()
    ac3._evtracker.wait_next_incoming_message()

    lp.sec("ac2: sending reply with a quote")
    reply_msg = Message.new_empty(msg.chat.account, "text")
    reply_msg.set_text("reply")
    reply_msg.quote = msg
    reply_msg = msg.chat.prepare_message(reply_msg)
    assert reply_msg.quoted_text == "hello"
    msg.chat.send_prepared(reply_msg)

    lp.sec("ac3: receiving reply")
    received_reply = ac3._evtracker.wait_next_incoming_message()
    assert received_reply.text == "reply"
    assert received_reply.quoted_text == "hello"
    # ac3 was not in the group and has not received quoted message
    assert received_reply.quote is None

    lp.sec("ac1: receiving reply")
    received_reply = ac1._evtracker.wait_next_incoming_message()
    assert received_reply.text == "reply"
    assert received_reply.quoted_text == "hello"
    assert received_reply.quote.id == out_msg.id


@pytest.mark.parametrize(
    "folder,move,expected_destination,",
    [
        (
            "xyz",
            False,
            "xyz",
        ),  # Test that emails are recognized in a random folder but not moved
        (
            "xyz",
            True,
            "DeltaChat",
        ),  # ...emails are found in a random folder and moved to DeltaChat
        (
            "Spam",
            False,
            "INBOX",
        ),  # ...emails are moved from the spam folder to the Inbox
    ],
)
# Testrun.org does not support the CREATE-SPECIAL-USE capability, which means that we can't create a folder with
# the "\Junk" flag (see https://tools.ietf.org/html/rfc6154). So, we can't test spam folder detection by flag.
def test_scan_folders(acfactory, lp, folder, move, expected_destination):
    """Delta Chat periodically scans all folders for new messages to make sure we don't miss any."""
    variant = folder + "-" + str(move) + "-" + expected_destination
    lp.sec("Testing variant " + variant)
    ac1 = acfactory.new_online_configuring_account(mvbox_move=move)
    ac2 = acfactory.new_online_configuring_account()

    acfactory.wait_configured(ac1)
    ac1.direct_imap.create_folder(folder)

    # Wait until each folder was selected once and we are IDLEing:
    acfactory.bring_accounts_online()
    ac1.stop_io()
    assert folder in ac1.direct_imap.list_folders()

    lp.sec("Send a message to from ac2 to ac1 and manually move it to the mvbox")
    ac1.direct_imap.select_config_folder("inbox")
    with ac1.direct_imap.idle() as idle1:
        acfactory.get_accepted_chat(ac2, ac1).send_text("hello")
        idle1.wait_for_new_message()
    ac1.direct_imap.conn.move(["*"], folder)  # "*" means "biggest UID in mailbox"

    lp.sec("start_io() and see if DeltaChat finds the message (" + variant + ")")
    ac1.set_config("scan_all_folders_debounce_secs", "0")
    ac1.start_io()
    msg = ac1._evtracker.wait_next_incoming_message()
    assert msg.text == "hello"

    # The message has been downloaded, which means it has reached its destination.
    ac1.direct_imap.select_folder(expected_destination)
    assert len(ac1.direct_imap.get_all_messages()) == 1
    if folder != expected_destination:
        ac1.direct_imap.select_folder(folder)
        assert len(ac1.direct_imap.get_all_messages()) == 0


def test_delete_deltachat_folder(acfactory):
    """Test that DeltaChat folder is recreated if user deletes it manually."""
    ac1 = acfactory.new_online_configuring_account(mvbox_move=True)
    ac2 = acfactory.new_online_configuring_account()
    acfactory.wait_configured(ac1)

    ac1.direct_imap.conn.folder.delete("DeltaChat")
    assert "DeltaChat" not in ac1.direct_imap.list_folders()
    acfactory.bring_accounts_online()

    ac2.create_chat(ac1).send_text("hello")
    msg = ac1._evtracker.wait_next_incoming_message()
    assert msg.text == "hello"

    assert "DeltaChat" in ac1.direct_imap.list_folders()


def test_aeap_flow_verified(acfactory, lp):
    """Test that a new address is added to a contact when it changes its address."""
    ac1, ac2, ac1new = acfactory.get_online_accounts(3)

    lp.sec("ac1: create verified-group QR, ac2 scans and joins")
    chat = ac1.create_group_chat("hello", verified=True)
    assert chat.is_protected()
    qr = chat.get_join_qr()
    lp.sec("ac2: start QR-code based join-group protocol")
    chat2 = ac2.qr_join_chat(qr)
    assert chat2.id >= 10
    ac1._evtracker.wait_securejoin_inviter_progress(1000)

    lp.sec("sending first message")
    msg_out = chat.send_text("old address")

    lp.sec("receiving first message")
    ac2._evtracker.wait_next_incoming_message()  # member added message
    msg_in_1 = ac2._evtracker.wait_next_incoming_message()
    assert msg_in_1.text == msg_out.text

    lp.sec("changing email account")
    ac1.set_config("addr", ac1new.get_config("addr"))
    ac1.set_config("mail_pw", ac1new.get_config("mail_pw"))
    ac1.stop_io()
    configtracker = ac1.configure()
    configtracker.wait_finish()
    ac1.start_io()

    lp.sec("sending second message")
    msg_out = chat.send_text("changed address")

    lp.sec("receiving second message")
    msg_in_2 = ac2._evtracker.wait_next_incoming_message()
    assert msg_in_2.text == msg_out.text
    assert msg_in_2.chat.id == msg_in_1.chat.id
    assert msg_in_2.get_sender_contact().addr == ac1new.get_config("addr")
    assert len(msg_in_2.chat.get_contacts()) == 2
    assert ac1new.get_config("addr") in [contact.addr for contact in msg_in_2.chat.get_contacts()]


class TestOnlineConfigureFails:
    def test_invalid_password(self, acfactory):
        configdict = acfactory.get_next_liveconfig()
        ac1 = acfactory.get_unconfigured_account()
        ac1.update_config(dict(addr=configdict["addr"], mail_pw="123"))
        configtracker = ac1.configure()
        configtracker.wait_progress(500)
        configtracker.wait_progress(0)

    def test_invalid_user(self, acfactory):
        configdict = acfactory.get_next_liveconfig()
        ac1 = acfactory.get_unconfigured_account()
        configdict["addr"] = "x" + configdict["addr"]
        ac1.update_config(configdict)
        configtracker = ac1.configure()
        configtracker.wait_progress(500)
        configtracker.wait_progress(0)

    def test_invalid_domain(self, acfactory):
        configdict = acfactory.get_next_liveconfig()
        ac1 = acfactory.get_unconfigured_account()
        configdict["addr"] += "x"
        ac1.update_config(configdict)
        configtracker = ac1.configure()
        configtracker.wait_progress(500)
        configtracker.wait_progress(0)
