//! Email accounts autoconfiguration process module.

mod auto_mozilla;
mod auto_outlook;
mod read_url;
mod server_params;

use anyhow::{bail, ensure, Context as _, Result};
use futures::FutureExt;
use futures_lite::FutureExt as _;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use tokio::task;

use crate::config::Config;
use crate::contact::addr_cmp;
use crate::context::Context;
use crate::imap::Imap;
use crate::job;
use crate::log::LogExt;
use crate::login_param::{CertificateChecks, LoginParam, ServerLoginParam, Socks5Config};
use crate::message::{Message, Viewtype};
use crate::oauth2::get_oauth2_addr;
use crate::provider::{Protocol, Socket, UsernamePattern};
use crate::scheduler::InterruptInfo;
use crate::smtp::Smtp;
use crate::stock_str;
use crate::tools::{time, EmailAddress};
use crate::{chat, e2ee, provider};

use auto_mozilla::moz_autoconfigure;
use auto_outlook::outlk_autodiscover;
use server_params::{expand_param_vector, ServerParams};

macro_rules! progress {
    ($context:tt, $progress:expr, $comment:expr) => {
        assert!(
            $progress <= 1000,
            "value in range 0..1000 expected with: 0=error, 1..999=progress, 1000=success"
        );
        $context.emit_event($crate::events::EventType::ConfigureProgress {
            progress: $progress,
            comment: $comment,
        });
    };
    ($context:tt, $progress:expr) => {
        progress!($context, $progress, None);
    };
}

impl Context {
    /// Checks if the context is already configured.
    pub async fn is_configured(&self) -> Result<bool> {
        self.sql
            .get_raw_config_bool("configured")
            .await
            .map_err(Into::into)
    }

    /// Configures this account with the currently set parameters.
    pub async fn configure(&self) -> Result<()> {
        ensure!(
            self.scheduler.read().await.is_none(),
            "cannot configure, already running"
        );
        ensure!(
            self.sql.is_open().await,
            "cannot configure, database not opened."
        );
        let cancel_channel = self.alloc_ongoing().await?;

        let res = self
            .inner_configure()
            .race(cancel_channel.recv().map(|_| {
                progress!(self, 0);
                Ok(())
            }))
            .await;

        self.free_ongoing().await;

        if let Err(err) = res.as_ref() {
            progress!(
                self,
                0,
                Some(
                    stock_str::configuration_failed(
                        self,
                        // We are using Anyhow's .context() and to show the
                        // inner error, too, we need the {:#}:
                        format!("{:#}", err),
                    )
                    .await
                )
            );
        } else {
            progress!(self, 1000);
        }

        res
    }

    async fn inner_configure(&self) -> Result<()> {
        info!(self, "Configure ...");

        let mut param = LoginParam::load_candidate_params(self).await?;
        let old_addr = self.get_config(Config::ConfiguredAddr).await?;
        let success = configure(self, &mut param).await;
        self.set_config(Config::NotifyAboutWrongPw, None).await?;

        on_configure_completed(self, param, old_addr).await?;

        success?;
        self.set_config(Config::NotifyAboutWrongPw, Some("1"))
            .await?;
        Ok(())
    }
}

async fn on_configure_completed(
    context: &Context,
    param: LoginParam,
    old_addr: Option<String>,
) -> Result<()> {
    if let Some(provider) = param.provider {
        if let Some(config_defaults) = &provider.config_defaults {
            for def in config_defaults.iter() {
                if !context.config_exists(def.key).await? {
                    info!(context, "apply config_defaults {}={}", def.key, def.value);
                    context.set_config(def.key, Some(def.value)).await?;
                } else {
                    info!(
                        context,
                        "skip already set config_defaults {}={}", def.key, def.value
                    );
                }
            }
        }

        if !provider.after_login_hint.is_empty() {
            let mut msg = Message::new(Viewtype::Text);
            msg.text = Some(provider.after_login_hint.to_string());
            if chat::add_device_msg(context, Some("core-provider-info"), Some(&mut msg))
                .await
                .is_err()
            {
                warn!(context, "cannot add after_login_hint as core-provider-info");
            }
        }
    }

    if let Some(new_addr) = context.get_config(Config::ConfiguredAddr).await? {
        if let Some(old_addr) = old_addr {
            if !addr_cmp(&new_addr, &old_addr) {
                let mut msg = Message::new(Viewtype::Text);
                msg.text =
                    Some(stock_str::aeap_explanation_and_link(context, old_addr, new_addr).await);
                chat::add_device_msg(context, None, Some(&mut msg))
                    .await
                    .ok_or_log_msg(context, "Cannot add AEAP explanation");
            }
        }
    }

    Ok(())
}

async fn configure(ctx: &Context, param: &mut LoginParam) -> Result<()> {
    progress!(ctx, 1);

    let socks5_config = param.socks5_config.clone();
    let socks5_enabled = socks5_config.is_some();

    let ctx2 = ctx.clone();
    let update_device_chats_handle = task::spawn(async move { ctx2.update_device_chats().await });

    // Step 1: Load the parameters and check email-address and password

    // Do oauth2 only if socks5 is disabled. As soon as we have a http library that can do
    // socks5 requests, this can work with socks5 too.  OAuth is always set either for both
    // IMAP and SMTP or not at all.
    if param.imap.oauth2 && !socks5_enabled {
        // the used oauth2 addr may differ, check this.
        // if get_oauth2_addr() is not available in the oauth2 implementation, just use the given one.
        progress!(ctx, 10);
        if let Some(oauth2_addr) = get_oauth2_addr(ctx, &param.addr, &param.imap.password)
            .await?
            .and_then(|e| e.parse().ok())
        {
            info!(ctx, "Authorized address is {}", oauth2_addr);
            param.addr = oauth2_addr;
            ctx.sql
                .set_raw_config("addr", Some(param.addr.as_str()))
                .await?;
        }
        progress!(ctx, 20);
    }
    // no oauth? - just continue it's no error

    let parsed: EmailAddress = param.addr.parse().context("Bad email-address")?;
    let param_domain = parsed.domain;
    let param_addr_urlencoded = utf8_percent_encode(&param.addr, NON_ALPHANUMERIC).to_string();

    // Step 2: Autoconfig
    progress!(ctx, 200);

    let param_autoconfig;
    if param.imap.server.is_empty()
        && param.imap.port == 0
        && param.imap.security == Socket::Automatic
        && param.imap.user.is_empty()
        && param.smtp.server.is_empty()
        && param.smtp.port == 0
        && param.smtp.security == Socket::Automatic
        && param.smtp.user.is_empty()
    {
        // no advanced parameters entered by the user: query provider-database or do Autoconfig

        info!(
            ctx,
            "checking internal provider-info for offline autoconfig"
        );

        if let Some(provider) =
            provider::get_provider_info(ctx, &param_domain, socks5_enabled).await
        {
            param.provider = Some(provider);
            match provider.status {
                provider::Status::Ok | provider::Status::Preparation => {
                    if provider.server.is_empty() {
                        info!(ctx, "offline autoconfig found, but no servers defined");
                        param_autoconfig = None;
                    } else {
                        info!(ctx, "offline autoconfig found");
                        let servers = provider
                            .server
                            .iter()
                            .map(|s| ServerParams {
                                protocol: s.protocol,
                                socket: s.socket,
                                hostname: s.hostname.to_string(),
                                port: s.port,
                                username: match s.username_pattern {
                                    UsernamePattern::Email => param.addr.to_string(),
                                    UsernamePattern::Emaillocalpart => {
                                        if let Some(at) = param.addr.find('@') {
                                            param.addr.split_at(at).0.to_string()
                                        } else {
                                            param.addr.to_string()
                                        }
                                    }
                                },
                                strict_tls: Some(provider.strict_tls),
                            })
                            .collect();

                        param_autoconfig = Some(servers)
                    }
                }
                provider::Status::Broken => {
                    info!(ctx, "offline autoconfig found, provider is broken");
                    param_autoconfig = None;
                }
            }
        } else {
            // Try receiving autoconfig
            info!(ctx, "no offline autoconfig found");
            param_autoconfig = if socks5_enabled {
                // Currently we can't do http requests through socks5, to not leak
                // the ip, just don't do online autoconfig
                info!(ctx, "socks5 enabled, skipping autoconfig");
                None
            } else {
                get_autoconfig(ctx, param, &param_domain, &param_addr_urlencoded).await
            }
        }
    } else {
        param_autoconfig = None;
    }

    progress!(ctx, 500);

    let mut servers = param_autoconfig.unwrap_or_default();
    if !servers
        .iter()
        .any(|server| server.protocol == Protocol::Imap)
    {
        servers.push(ServerParams {
            protocol: Protocol::Imap,
            hostname: param.imap.server.clone(),
            port: param.imap.port,
            socket: param.imap.security,
            username: param.imap.user.clone(),
            strict_tls: None,
        })
    }
    if !servers
        .iter()
        .any(|server| server.protocol == Protocol::Smtp)
    {
        servers.push(ServerParams {
            protocol: Protocol::Smtp,
            hostname: param.smtp.server.clone(),
            port: param.smtp.port,
            socket: param.smtp.security,
            username: param.smtp.user.clone(),
            strict_tls: None,
        })
    }

    // respect certificate setting from function parameters
    for mut server in &mut servers {
        let certificate_checks = match server.protocol {
            Protocol::Imap => param.imap.certificate_checks,
            Protocol::Smtp => param.smtp.certificate_checks,
        };
        server.strict_tls = match certificate_checks {
            CertificateChecks::AcceptInvalidCertificates
            | CertificateChecks::AcceptInvalidCertificates2 => Some(false),
            CertificateChecks::Strict => Some(true),
            CertificateChecks::Automatic => server.strict_tls,
        };
    }

    let servers = expand_param_vector(servers, &param.addr, &param_domain);

    progress!(ctx, 550);

    // Spawn SMTP configuration task
    let mut smtp = Smtp::new();

    let context_smtp = ctx.clone();
    let mut smtp_param = param.smtp.clone();
    let smtp_addr = param.addr.clone();
    let smtp_servers: Vec<ServerParams> = servers
        .iter()
        .filter(|params| params.protocol == Protocol::Smtp)
        .cloned()
        .collect();
    let provider_strict_tls = param
        .provider
        .map_or(socks5_config.is_some(), |provider| provider.strict_tls);

    let smtp_config_task = task::spawn(async move {
        let mut smtp_configured = false;
        let mut errors = Vec::new();
        for smtp_server in smtp_servers {
            smtp_param.user = smtp_server.username.clone();
            smtp_param.server = smtp_server.hostname.clone();
            smtp_param.port = smtp_server.port;
            smtp_param.security = smtp_server.socket;
            smtp_param.certificate_checks = match smtp_server.strict_tls {
                Some(true) => CertificateChecks::Strict,
                Some(false) => CertificateChecks::AcceptInvalidCertificates,
                None => CertificateChecks::Automatic,
            };

            match try_smtp_one_param(
                &context_smtp,
                &smtp_param,
                &socks5_config,
                &smtp_addr,
                provider_strict_tls,
                &mut smtp,
            )
            .await
            {
                Ok(_) => {
                    smtp_configured = true;
                    break;
                }
                Err(e) => errors.push(e),
            }
        }

        if smtp_configured {
            Ok(smtp_param)
        } else {
            Err(errors)
        }
    });

    progress!(ctx, 600);

    // Configure IMAP

    let mut imap: Option<Imap> = None;
    let imap_servers: Vec<&ServerParams> = servers
        .iter()
        .filter(|params| params.protocol == Protocol::Imap)
        .collect();
    let imap_servers_count = imap_servers.len();
    let mut errors = Vec::new();
    for (imap_server_index, imap_server) in imap_servers.into_iter().enumerate() {
        param.imap.user = imap_server.username.clone();
        param.imap.server = imap_server.hostname.clone();
        param.imap.port = imap_server.port;
        param.imap.security = imap_server.socket;
        param.imap.certificate_checks = match imap_server.strict_tls {
            Some(true) => CertificateChecks::Strict,
            Some(false) => CertificateChecks::AcceptInvalidCertificates,
            None => CertificateChecks::Automatic,
        };

        match try_imap_one_param(
            ctx,
            &param.imap,
            &param.socks5_config,
            &param.addr,
            provider_strict_tls,
        )
        .await
        {
            Ok(configured_imap) => {
                imap = Some(configured_imap);
                break;
            }
            Err(e) => errors.push(e),
        }
        progress!(
            ctx,
            600 + (800 - 600) * (1 + imap_server_index) / imap_servers_count
        );
    }
    let mut imap = match imap {
        Some(imap) => imap,
        None => bail!(nicer_configuration_error(ctx, errors).await),
    };

    progress!(ctx, 850);

    // Wait for SMTP configuration
    match smtp_config_task.await.unwrap() {
        Ok(smtp_param) => {
            param.smtp = smtp_param;
        }
        Err(errors) => {
            bail!(nicer_configuration_error(ctx, errors).await);
        }
    }

    progress!(ctx, 900);

    let create_mvbox = ctx.should_watch_mvbox().await?;

    // Send client ID as soon as possible before doing anything else.
    imap.determine_capabilities(ctx).await?;

    imap.configure_folders(ctx, create_mvbox).await?;

    imap.select_with_uidvalidity(ctx, "INBOX")
        .await
        .context("could not read INBOX status")?;

    drop(imap);

    progress!(ctx, 910);

    if ctx.get_config(Config::ConfiguredAddr).await?.as_deref() != Some(param.addr.as_str()) {
        // Switched account, all server UIDs we know are invalid
        job::schedule_resync(ctx).await?;
    }

    // the trailing underscore is correct
    param.save_as_configured_params(ctx).await?;
    ctx.set_config(Config::ConfiguredTimestamp, Some(&time().to_string()))
        .await?;

    progress!(ctx, 920);

    e2ee::ensure_secret_key_exists(ctx).await?;
    info!(ctx, "key generation completed");

    ctx.set_config_bool(Config::FetchedExistingMsgs, false)
        .await?;
    ctx.interrupt_inbox(InterruptInfo::new(false)).await;

    progress!(ctx, 940);
    update_device_chats_handle.await??;

    ctx.sql.set_raw_config_bool("configured", true).await?;

    Ok(())
}

/// Retrieve available autoconfigurations.
///
/// A Search configurations from the domain used in the email-address, prefer encrypted
/// B. If we have no configuration yet, search configuration in Thunderbird's centeral database
async fn get_autoconfig(
    ctx: &Context,
    param: &LoginParam,
    param_domain: &str,
    param_addr_urlencoded: &str,
) -> Option<Vec<ServerParams>> {
    if let Ok(res) = moz_autoconfigure(
        ctx,
        &format!(
            "https://autoconfig.{}/mail/config-v1.1.xml?emailaddress={}",
            param_domain, param_addr_urlencoded
        ),
        param,
    )
    .await
    {
        return Some(res);
    }
    progress!(ctx, 300);

    if let Ok(res) = moz_autoconfigure(
        ctx,
        // the doc does not mention `emailaddress=`, however, Thunderbird adds it, see <https://releases.mozilla.org/pub/thunderbird/>,  which makes some sense
        &format!(
            "https://{}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={}",
            &param_domain, &param_addr_urlencoded
        ),
        param,
    )
    .await
    {
        return Some(res);
    }
    progress!(ctx, 310);

    // Outlook uses always SSL but different domains (this comment describes the next two steps)
    if let Ok(res) = outlk_autodiscover(
        ctx,
        format!("https://{}/autodiscover/autodiscover.xml", &param_domain),
    )
    .await
    {
        return Some(res);
    }
    progress!(ctx, 320);

    if let Ok(res) = outlk_autodiscover(
        ctx,
        format!(
            "https://autodiscover.{}/autodiscover/autodiscover.xml",
            &param_domain
        ),
    )
    .await
    {
        return Some(res);
    }
    progress!(ctx, 330);

    // always SSL for Thunderbird's database
    if let Ok(res) = moz_autoconfigure(
        ctx,
        &format!("https://autoconfig.thunderbird.net/v1.1/{}", &param_domain),
        param,
    )
    .await
    {
        return Some(res);
    }

    None
}

async fn try_imap_one_param(
    context: &Context,
    param: &ServerLoginParam,
    socks5_config: &Option<Socks5Config>,
    addr: &str,
    provider_strict_tls: bool,
) -> Result<Imap, ConfigurationError> {
    let inf = format!(
        "imap: {}@{}:{} security={} certificate_checks={} oauth2={}",
        param.user,
        param.server,
        param.port,
        param.security,
        param.certificate_checks,
        param.oauth2
    );
    info!(context, "Trying: {}", inf);

    let (_s, r) = async_channel::bounded(1);

    let mut imap = match Imap::new(param, socks5_config.clone(), addr, provider_strict_tls, r).await
    {
        Err(err) => {
            info!(context, "failure: {}", err);
            return Err(ConfigurationError {
                config: inf,
                msg: err.to_string(),
            });
        }
        Ok(imap) => imap,
    };

    match imap.connect(context).await {
        Err(err) => {
            info!(context, "failure: {}", err);
            Err(ConfigurationError {
                config: inf,
                msg: err.to_string(),
            })
        }
        Ok(()) => {
            info!(context, "success: {}", inf);
            Ok(imap)
        }
    }
}

async fn try_smtp_one_param(
    context: &Context,
    param: &ServerLoginParam,
    socks5_config: &Option<Socks5Config>,
    addr: &str,
    provider_strict_tls: bool,
    smtp: &mut Smtp,
) -> Result<(), ConfigurationError> {
    let inf = format!(
        "smtp: {}@{}:{} security={} certificate_checks={} oauth2={} socks5_config={}",
        param.user,
        param.server,
        param.port,
        param.security,
        param.certificate_checks,
        param.oauth2,
        if let Some(socks5_config) = socks5_config {
            socks5_config.to_string()
        } else {
            "None".to_string()
        }
    );
    info!(context, "Trying: {}", inf);

    if let Err(err) = smtp
        .connect(context, param, socks5_config, addr, provider_strict_tls)
        .await
    {
        info!(context, "failure: {}", err);
        Err(ConfigurationError {
            config: inf,
            msg: err.to_string(),
        })
    } else {
        info!(context, "success: {}", inf);
        smtp.disconnect().await;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Trying {config}…\nError: {msg}")]
pub struct ConfigurationError {
    config: String,
    msg: String,
}

async fn nicer_configuration_error(context: &Context, errors: Vec<ConfigurationError>) -> String {
    let first_err = if let Some(f) = errors.first() {
        f
    } else {
        // This means configuration failed but no errors have been captured. This should never
        // happen, but if it does, the user will see classic "Error: no error".
        return "no error".to_string();
    };

    if errors.iter().all(|e| {
        e.msg.to_lowercase().contains("could not resolve")
            || e.msg
                .to_lowercase()
                .contains("temporary failure in name resolution")
            || e.msg.to_lowercase().contains("name or service not known")
            || e.msg
                .to_lowercase()
                .contains("failed to lookup address information")
    }) {
        return stock_str::error_no_network(context).await;
    }

    if errors.iter().all(|e| e.msg == first_err.msg) {
        return first_err.msg.to_string();
    }

    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<String>>()
        .join("\n\n")
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid email address: {0:?}")]
    InvalidEmailAddress(String),

    #[error("XML error at position {position}: {error}")]
    InvalidXml {
        position: usize,
        #[source]
        error: quick_xml::Error,
    },

    #[error("Number of redirection is exceeded")]
    Redirection,

    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]

    use crate::config::Config;
    use crate::test_utils::TestContext;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_no_panic_on_bad_credentials() {
        let t = TestContext::new().await;
        t.set_config(Config::Addr, Some("probably@unexistant.addr"))
            .await
            .unwrap();
        t.set_config(Config::MailPw, Some("123456")).await.unwrap();
        assert!(t.configure().await.is_err());
    }
}
