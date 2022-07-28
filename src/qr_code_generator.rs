use anyhow::Result;
use qrcodegen::{QrCode, QrCodeEcc};

use crate::{
    blob::BlobObject,
    chat::{Chat, ChatId},
    color::color_int_to_hex_string,
    config::Config,
    contact::{Contact, ContactId},
    context::Context,
    qr::DCBACKUP_SCHEME,
    securejoin, stock_str,
};

pub async fn get_securejoin_qr_svg(context: &Context, chat_id: Option<ChatId>) -> Result<String> {
    if let Some(chat_id) = chat_id {
        generate_join_group_qr_code(context, chat_id).await
    } else {
        generate_verification_qr(context).await
    }
}

async fn generate_join_group_qr_code(context: &Context, chat_id: ChatId) -> Result<String> {
    let chat = Chat::load_from_db(context, chat_id).await?;

    let avatar = match chat.get_profile_image(context).await? {
        Some(path) => {
            let avatar_blob = BlobObject::from_path(context, &path)?;
            Some(tokio::fs::read(avatar_blob.to_abs_path()).await?)
        }
        None => None,
    };

    inner_generate_secure_join_qr_code(
        &stock_str::secure_join_group_qr_description(context, &chat).await,
        &securejoin::get_securejoin_qr(context, Some(chat_id)).await?,
        &color_int_to_hex_string(chat.get_color(context).await?),
        avatar,
        chat.get_name().chars().next().unwrap_or('#'),
    )
}

async fn generate_verification_qr(context: &Context) -> Result<String> {
    let contact = Contact::get_by_id(context, ContactId::SELF).await?;

    let avatar = match contact.get_profile_image(context).await? {
        Some(path) => {
            let avatar_blob = BlobObject::from_path(context, &path)?;
            Some(tokio::fs::read(avatar_blob.to_abs_path()).await?)
        }
        None => None,
    };

    let displayname = match context.get_config(Config::Displayname).await? {
        Some(name) => name,
        None => contact.get_addr().to_owned(),
    };

    inner_generate_secure_join_qr_code(
        &stock_str::setup_contact_qr_description(context, &displayname, contact.get_addr()).await,
        &securejoin::get_securejoin_qr(context, None).await?,
        &color_int_to_hex_string(contact.get_color()),
        avatar,
        displayname.chars().next().unwrap_or('#'),
    )
}

fn inner_generate_secure_join_qr_code(
    qrcode_description: &str,
    qrcode_content: &str,
    color: &str,
    avatar: Option<Vec<u8>>,
    avatar_letter: char,
) -> Result<String> {
    // config
    let width = 515.0;
    let height = 630.0;
    let logo_offset = 28.0;
    let qr_code_size = 400.0;
    let qr_translate_up = 40.0;
    let text_y_pos = ((height - qr_code_size) / 2.0) + qr_code_size;
    let avatar_border_size = 9.0;
    let card_border_size = 2.0;
    let card_roundness = 40.0;

    let qr = QrCode::encode_text(qrcode_content, QrCodeEcc::Medium)?;
    let mut svg = String::with_capacity(28000);
    let mut w = tagger::new(&mut svg);

    w.elem("svg", |d| {
        d.attr("xmlns", "http://www.w3.org/2000/svg")?;
        d.attr("viewBox", format_args!("0 0 {} {}", width, height))?;
        Ok(())
    })?
    .build(|w| {
        // White Background apears like a card
        w.single("rect", |d| {
            d.attr("x", card_border_size)?;
            d.attr("y", card_border_size)?;
            d.attr("rx", card_roundness)?;
            d.attr("stroke", "#c6c6c6")?;
            d.attr("stroke-width", card_border_size)?;
            d.attr("width", width - (card_border_size * 2.0))?;
            d.attr("height", height - (card_border_size * 2.0))?;
            d.attr("style", "fill:#f2f2f2")?;
            Ok(())
        })?;
        // Qrcode
        w.elem("g", |d| {
            d.attr(
                "transform",
                format!(
                    "translate({},{})",
                    (width - qr_code_size) / 2.0,
                    ((height - qr_code_size) / 2.0) - qr_translate_up
                ),
            )
            // If the qr code should be in the wrong place,
            // we could also translate and scale the points in the path already,
            // but that would make the resulting svg way bigger in size and might bring up rounding issues,
            // so better avoid doing it manually if possible
        })?
        .build(|w| {
            w.single("path", |d| {
                let mut path_data = String::with_capacity(0);
                let scale = qr_code_size / qr.size() as f32;

                for y in 0..qr.size() {
                    for x in 0..qr.size() {
                        if qr.get_module(x, y) {
                            path_data += &format!("M{},{}h1v1h-1z", x, y);
                        }
                    }
                }

                d.attr("style", "fill:#000000")?;
                d.attr("d", path_data)?;
                d.attr("transform", format!("scale({})", scale))
            })
        })?;

        // Text
        const BIG_TEXT_CHARS_PER_LINE: usize = 32;
        const SMALL_TEXT_CHARS_PER_LINE: usize = 38;
        let chars_per_line = if qrcode_description.len() > SMALL_TEXT_CHARS_PER_LINE * 2 {
            SMALL_TEXT_CHARS_PER_LINE
        } else {
            BIG_TEXT_CHARS_PER_LINE
        };
        let lines = textwrap::fill(qrcode_description, chars_per_line);
        let (text_font_size, text_y_shift) = if lines.split('\n').count() <= 2 {
            (27.0, 0.0)
        } else {
            (19.0, -10.0)
        };
        for (count, line) in lines.split('\n').enumerate() {
            w.elem("text", |d| {
                d.attr(
                    "y",
                    (count as f32 * (text_font_size * 1.2)) + text_y_pos + text_y_shift,
                )?;
                d.attr("x", width / 2.0)?;
                d.attr("text-anchor", "middle")?;
                d.attr(
                    "style",
                    format!(
                        "font-family:sans-serif;\
                        font-weight:bold;\
                        font-size:{}px;\
                        fill:#000000;\
                        stroke:none",
                        text_font_size
                    ),
                )
            })?
            .build(|w| w.put_raw(line))?;
        }
        // contact avatar in middle of qrcode
        const LOGO_SIZE: f32 = 94.4;
        const HALF_LOGO_SIZE: f32 = LOGO_SIZE / 2.0;
        let logo_position_in_qr = (qr_code_size / 2.0) - HALF_LOGO_SIZE;
        let logo_position_x = ((width - qr_code_size) / 2.0) + logo_position_in_qr;
        let logo_position_y =
            ((height - qr_code_size) / 2.0) - qr_translate_up + logo_position_in_qr;

        w.single("circle", |d| {
            d.attr("cx", logo_position_x + HALF_LOGO_SIZE)?;
            d.attr("cy", logo_position_y + HALF_LOGO_SIZE)?;
            d.attr("r", HALF_LOGO_SIZE + avatar_border_size)?;
            d.attr("style", "fill:#f2f2f2")
        })?;

        if let Some(img) = avatar {
            w.elem("defs", tagger::no_attr())?.build(|w| {
                w.elem("clipPath", |d| d.attr("id", "avatar-cut"))?
                    .build(|w| {
                        w.single("circle", |d| {
                            d.attr("cx", logo_position_x + HALF_LOGO_SIZE)?;
                            d.attr("cy", logo_position_y + HALF_LOGO_SIZE)?;
                            d.attr("r", HALF_LOGO_SIZE)
                        })
                    })
            })?;

            w.single("image", |d| {
                d.attr("x", logo_position_x)?;
                d.attr("y", logo_position_y)?;
                d.attr("width", HALF_LOGO_SIZE * 2.0)?;
                d.attr("height", HALF_LOGO_SIZE * 2.0)?;
                d.attr("preserveAspectRatio", "none")?;
                d.attr("clip-path", "url(#avatar-cut)")?;
                d.attr(
                    "href", /*might need xlink:href instead if it doesn't work on older devices?*/
                    format!("data:image/jpeg;base64,{}", base64::encode(img)),
                )
            })?;
        } else {
            w.single("circle", |d| {
                d.attr("cx", logo_position_x + HALF_LOGO_SIZE)?;
                d.attr("cy", logo_position_y + HALF_LOGO_SIZE)?;
                d.attr("r", HALF_LOGO_SIZE)?;
                d.attr("style", format!("fill:{}", &color))
            })?;

            let avatar_font_size = LOGO_SIZE * 0.65;
            let font_offset = avatar_font_size * 0.1;
            w.elem("text", |d| {
                d.attr("y", logo_position_y + HALF_LOGO_SIZE + font_offset)?;
                d.attr("x", logo_position_x + HALF_LOGO_SIZE)?;
                d.attr("text-anchor", "middle")?;
                d.attr("dominant-baseline", "central")?;
                d.attr("alignment-baseline", "middle")?;
                d.attr(
                    "style",
                    format!(
                        "font-family:sans-serif;\
                            font-weight:400;\
                            font-size:{}px;\
                            fill:#ffffff;",
                        avatar_font_size
                    ),
                )
            })?
            .build(|w| w.put_raw(avatar_letter.to_uppercase()))?;
        }

        // Footer logo
        const FOOTER_HEIGHT: f32 = 35.0;
        const FOOTER_WIDTH: f32 = 198.0;
        w.elem("g", |d| {
            d.attr(
                "transform",
                format!(
                    "translate({},{})",
                    (width - FOOTER_WIDTH) / 2.0,
                    height - logo_offset - FOOTER_HEIGHT - text_y_shift
                ),
            )
        })?
        .build(|w| w.put_raw(include_str!("../assets/qrcode_logo_footer.svg")))
    })?;

    Ok(svg)
}

pub fn generate_backup_qr_code(ticket: &iroh_share::Ticket) -> Result<String> {
    let ticket_bytes = ticket.as_bytes();
    let ticket_str = multibase::encode(multibase::Base::Base64, &ticket_bytes);
    let ticket_str = format!("{}{}", DCBACKUP_SCHEME, ticket_str);
    // config
    let width = 515.0;
    let height = 630.0;
    let qr_code_size = 400.0;
    let qr_translate_up = 40.0;
    let card_roundness = 40.0;
    let card_border_size = 2.0;

    let qr = QrCode::encode_text(&ticket_str, QrCodeEcc::Medium)?;
    let mut svg = String::with_capacity(28000);
    let mut w = tagger::new(&mut svg);

    w.elem("svg", |d| {
        d.attr("xmlns", "http://www.w3.org/2000/svg")?;
        d.attr("viewBox", format_args!("0 0 {} {}", width, height))?;
        Ok(())
    })?
    .build(|w| {
        // White Background apears like a card
        w.single("rect", |d| {
            d.attr("x", card_border_size)?;
            d.attr("y", card_border_size)?;
            d.attr("rx", card_roundness)?;
            d.attr("stroke", "#c6c6c6")?;
            d.attr("stroke-width", card_border_size)?;
            d.attr("width", width - (card_border_size * 2.0))?;
            d.attr("height", height - (card_border_size * 2.0))?;
            d.attr("style", "fill:#f2f2f2")?;
            Ok(())
        })?;
        // Qrcode
        w.elem("g", |d| {
            d.attr(
                "transform",
                format!(
                    "translate({},{})",
                    (width - qr_code_size) / 2.0,
                    ((height - qr_code_size) / 2.0) - qr_translate_up
                ),
            )
            // If the qr code should be in the wrong place,
            // we could also translate and scale the points in the path already,
            // but that would make the resulting svg way bigger in size and might bring up rounding issues,
            // so better avoid doing it manually if possible
        })?
        .build(|w| {
            w.single("path", |d| {
                let mut path_data = String::with_capacity(0);
                let scale = qr_code_size / qr.size() as f32;

                for y in 0..qr.size() {
                    for x in 0..qr.size() {
                        if qr.get_module(x, y) {
                            path_data += &format!("M{},{}h1v1h-1z", x, y);
                        }
                    }
                }

                d.attr("style", "fill:#000000")?;
                d.attr("d", path_data)?;
                d.attr("transform", format!("scale({})", scale))
            })
        })?;

        // Footer logo
        const FOOTER_HEIGHT: f32 = 35.0;
        const FOOTER_WIDTH: f32 = 198.0;
        w.elem("g", |d| {
            d.attr(
                "transform",
                format!(
                    "translate({},{})",
                    (width - FOOTER_WIDTH) / 2.0,
                    height - FOOTER_HEIGHT
                ),
            )
        })?
        .build(|w| w.put_raw(include_str!("../assets/qrcode_logo_footer.svg")))
    })?;

    Ok(svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_svg_escaping() {
        let svg = inner_generate_secure_join_qr_code(
            "descr123 \" < > &",
            "qr-code-content",
            "#000000",
            None,
            'X',
        )
        .unwrap();
        assert!(svg.contains("descr123 &quot; &lt; &gt; &amp;"))
    }
}
