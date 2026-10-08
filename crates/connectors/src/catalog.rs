//! Connector catalog — port of `coworker/connectors/descriptors.py` +
//! `coworker/connectors/catalog_copy.py` (about/access copy).
//!
//! Data that drives the guided setup wizard: auth method, setup fields,
//! instructions, and registry metadata for every connector (including placeholders).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub placeholder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorDescriptor {
    pub name: String,
    pub title: String,
    pub icon: String,
    pub blurb: String,
    pub auth: String,
    pub two_way: bool,
    pub fields: Vec<FieldDef>,
    pub instructions: Vec<String>,
    #[serde(default = "default_true")]
    pub available: bool,
    #[serde(default)]
    pub channels: bool,
    #[serde(default = "default_gray")]
    pub brand_color: String,
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub mcp_url: String,
    #[serde(default)]
    pub experimental: bool,
    #[serde(default)]
    pub risk_notice: String,
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub managed_paused: bool,
    #[serde(default)]
    pub account_field: String,
    #[serde(default)]
    pub about: String,
    #[serde(default)]
    pub access: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthKind {
    None,
    Manual,
    ManagedOAuth { provider: &'static str },
    McpOAuth { url: String },
}

fn default_true() -> bool {
    true
}

fn default_gray() -> String {
    "#6b7280".to_string()
}

macro_rules! sv {
    ($($x:expr),* $(,)?) => {
        vec![$( $x.to_string() ),*]
    };
}

impl Default for ConnectorDescriptor {
    fn default() -> Self {
        Self {
            name: String::new(),
            title: String::new(),
            icon: String::new(),
            blurb: String::new(),
            auth: String::new(),
            two_way: false,
            fields: Vec::new(),
            instructions: Vec::new(),
            available: true,
            channels: false,
            brand_color: default_gray(),
            logo: String::new(),
            aliases: Vec::new(),
            mcp_url: String::new(),
            experimental: false,
            risk_notice: String::new(),
            managed: false,
            managed_paused: false,
            account_field: String::new(),
            about: String::new(),
            access: Vec::new(),
        }
    }
}


// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

fn field(
    key: &str,
    label: &str,
    secret: bool,
    required: bool,
    help: &str,
    placeholder: &str,
) -> FieldDef {
    FieldDef {
        key: key.into(),
        label: label.into(),
        secret,
        required,
        help: help.into(),
        placeholder: placeholder.into(),
    }
}

/// Same as Python `_ALLOWED_FIELD`.
fn allowed_field() -> FieldDef {
    field(
        "allowed_users",
        "Allowed user IDs",
        false,
        false,
        "Comma-separated IDs allowed to message the bot. Leave empty, then DM the bot and use Capture.",
        "123456789",
    )
}

// ---------------------------------------------------------------------------
// Managed OAuth provider mapping (mirrors `coworker/cloud.py`)
// ---------------------------------------------------------------------------

fn provider_for_connector(name: &str) -> Option<&'static str> {
    match name {
        "gmail" | "google_calendar" | "google_drive" => Some("google"),
        "outlook" => Some("microsoft"),
        "slack" => Some("slack"),
        "notion" => Some("notion"),
        "attio" => Some("attio"),
        "hubspot" => Some("hubspot"),
        "github" => Some("github"),
        _ => None,
    }
}

/// Classify how a connector authenticates.
pub fn auth_kind(d: &ConnectorDescriptor) -> AuthKind {
    if d.auth == "none" {
        return AuthKind::None;
    }
    if !d.mcp_url.is_empty() {
        return AuthKind::McpOAuth {
            url: d.mcp_url.clone(),
        };
    }
    if d.managed {
        if let Some(provider) = provider_for_connector(&d.name) {
            return AuthKind::ManagedOAuth { provider };
        }
    }
    AuthKind::Manual
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

fn build_descriptors() -> Vec<ConnectorDescriptor> {
    vec![
    ConnectorDescriptor {
        name: "telegram".into(),
        title: "Telegram".into(),
        icon: "✈".into(),
        blurb: "Two-way messaging with a Telegram bot.".into(),
        auth: "bot_token".into(),
        two_way: true,
        fields: vec![
            field("bot_token", "Bot token", true, true, "From @BotFather.", "123456:ABC-DEF…"),
            allowed_field(),
        ],
        instructions: sv![
            "Open Telegram and message @BotFather.",
            "Send /newbot and pick a name + username.",
            "Copy the HTTP API token it gives you and paste it below.",
            "After connecting, DM your new bot once, then use Capture to grab your user ID."
        ],
        channels: true,
        brand_color: "#229ed9".into(),
        logo: "telegram".into(),
        about: "Chat with your coworker from Telegram. Messages to your bot reach the agent and replies come back to the same chat — only senders on your allow-list get through.".into(),
        access: sv!["Reads messages sent to your bot — never your personal chats.", "Sends messages as the bot.", "Only senders on your allow-list are answered."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "slack".into(),
        title: "Slack".into(),
        icon: "💬".into(),
        blurb: "Two-way messaging — one-click via OpenWorker Cloud, or a manual Slack app (Socket Mode).".into(),
        auth: "socket_app".into(),
        two_way: true,
        fields: vec![
            field("bot_token", "Bot token", true, true, "Bot User OAuth Token.", "xoxb-…"),
            field("app_token", "App token", true, true, "App-level token for Socket Mode.", "xapp-…"),
            allowed_field(),
        ],
        instructions: sv![
            "Go to api.slack.com/apps → Create New App (from scratch).",
            "Settings → Socket Mode: enable it and generate an app-level token (xapp-) with connections:write.",
            "Features → Interactivity & Shortcuts: turn Interactivity ON (no Request URL needed in Socket Mode) — required for Approve/Deny buttons.",
            "OAuth & Permissions: add bot scopes chat:write, files:write, app_mentions:read, im:history, channels:history, groups:history, users:read, channels:read, groups:read (files:write lets the agent send files; the last three resolve sender/channel display names).",
            "Install to workspace and copy the Bot User OAuth Token (xoxb-).",
            "Paste both tokens below and Connect, then invite the bot to a channel or DM it."
        ],
        channels: true,
        brand_color: "#611f69".into(),
        logo: "slack".into(),
        managed: true,
        about: "Bring your coworker into Slack: mention it in a channel or DM it, and replies land in-thread. Any number of workspaces can be connected, each with its own allow-list of who may talk to the agent.".into(),
        access: sv!["Reads channels the bot is invited to, and its DMs.", "Posts messages and uploads files as the bot.", "Reads files shared in those channels.", "Reads member and channel names to resolve who's talking."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "email".into(),
        title: "Email (IMAP)".into(),
        icon: "✉".into(),
        blurb: "Read, search, and send mail from any IMAP account — Gmail, iCloud, Fastmail, or custom.".into(),
        auth: "app_password".into(),
        two_way: false,
        fields: vec![
            field("address", "Email address", false, true, "", "you@gmail.com"),
            field("app_password", "App password", true, true, "Gmail/iCloud: generate an app password (requires 2-step verification). Not your account password.", ""),
            field("display_name", "Display name", false, false, "Shown as the From name on sent mail.", ""),
            field("imap_host", "IMAP host (advanced)", false, false, "Only needed for providers we don't auto-detect.", "imap.example.com"),
            field("imap_port", "IMAP port (advanced)", false, false, "", "993"),
            field("smtp_host", "SMTP host (advanced)", false, false, "", "smtp.example.com"),
            field("smtp_port", "SMTP port (advanced)", false, false, "", "587"),
        ],
        instructions: sv![
            "Gmail: turn on 2-Step Verification, then create an app password at myaccount.google.com/apppasswords.",
            "iCloud: generate an app-specific password at account.apple.com → Sign-In and Security.",
            "Enter your address and the app password below. Gmail, iCloud, and Fastmail servers are detected automatically; for other providers fill in the IMAP/SMTP hosts.",
            "Note: Google Workspace and Microsoft 365 accounts often have IMAP or app passwords disabled by the org admin."
        ],
        logo: "email".into(),
        about: "Read, search, and send mail on any IMAP account — Gmail, iCloud, Fastmail, or your own server — using an app password instead of your account password.".into(),
        access: sv!["Reads and searches mail over IMAP.", "Sends mail as your address, and saves attachments locally.", "Signs in with an app password — never your account password."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "gmail".into(),
        title: "Gmail".into(),
        icon: "✉".into(),
        blurb: "Search, summarize, draft, and send email.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Google OAuth token with Gmail scopes.", ""),
        ],
        instructions: sv![
            "Use a Google OAuth access token with Gmail readonly and send scopes.",
            "Paste the access token below."
        ],
        brand_color: "#ea4335".into(),
        logo: "gmail".into(),
        aliases: sv!["email", "mail", "google"],
        managed: true,
        managed_paused: true,
        about: "Search, summarize, and send over your Gmail. Multiple accounts connect side by side, and privacy filters can hide chosen senders or labels from agents entirely.".into(),
        access: sv!["Reads and searches your mail.", "Sends email as you.", "Never deletes mail or changes account settings."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "google_calendar".into(),
        title: "Google Calendar".into(),
        icon: "◷".into(),
        blurb: "Read availability, summarize schedules, and create events.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Google OAuth token with Calendar scopes.", ""),
        ],
        instructions: sv![
            "Use a Google OAuth access token with Calendar read/write scopes.",
            "Paste the access token below."
        ],
        brand_color: "#4285f4".into(),
        logo: "google_calendar".into(),
        managed: true,
        managed_paused: true,
        about: "Check availability, summarize your week, and manage events. Multiple Google accounts connect side by side.".into(),
        access: sv!["Reads events and availability across your calendars.", "Creates, updates, and deletes events."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "browser".into(),
        title: "Browser".into(),
        icon: "⌕".into(),
        blurb: "Let agents navigate, read, and act on websites with approval.".into(),
        auth: "none".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![
            "No setup required. Browser tools are available to Cowork sessions."
        ],
        brand_color: "#0ea5e9".into(),
        logo: "browser".into(),
        about: "A built-in browser agents drive to read pages and act on websites — separate from your personal browser, with actions subject to approval.".into(),
        access: sv!["Opens and reads web pages in its own browser session.", "Clicks, types, and uploads files only inside that session.", "Never touches your personal browser or its logins."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "github".into(),
        title: "GitHub".into(),
        icon: "⌘".into(),
        blurb: "Work with issues, pull requests, repository files, and CI status.".into(),
        auth: "token".into(),
        two_way: true,
        fields: vec![
            field("token", "Personal access token", true, true, "Fine-grained or classic GitHub token.", ""),
        ],
        instructions: sv![
            "Create a GitHub personal access token with access to the target repositories.",
            "For write actions, include Issues or Pull Requests write permissions as needed."
        ],
        brand_color: "#1f2328".into(),
        logo: "github".into(),
        managed: true,
        about: "Work with issues, pull requests, repository files, and CI status. One click installs the OpenWorker GitHub App on the repositories you pick; mention the agent on an issue or PR and it answers from your desktop.".into(),
        access: sv!["Reads code, issues, pull requests, and CI on repositories you grant.", "Creates issues, replies, and reviews pull requests.", "You pick the repositories on GitHub — one, several, or all."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "outlook".into(),
        title: "Outlook".into(),
        icon: "◎".into(),
        blurb: "Microsoft 365 mail and calendar: search, draft, and send email; manage events and respond to invites.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Microsoft Graph access token.", ""),
        ],
        instructions: sv![
            "One click connects via OpenWorker Cloud (recommended).",
            "Manual: paste a Microsoft Graph access token with Mail and Calendar scopes."
        ],
        brand_color: "#0078d4".into(),
        logo: "outlook".into(),
        aliases: sv!["calendar", "email", "mail", "microsoft", "office"],
        managed: true,
        account_field: "@identity".into(),
        about: "Search, summarize, and send Microsoft 365 mail, and run your calendar — create and move meetings, respond to invites. Multiple mailboxes connect side by side.".into(),
        access: sv!["Reads and searches your mail.", "Sends mail as you.", "Reads your calendar.", "Creates, changes, and cancels events; responds to invites as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "jira".into(),
        title: "Jira".into(),
        icon: "◆".into(),
        blurb: "Search, summarize, create, and update issues.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("base_url", "Atlassian site URL", false, true, "Example: https://example.atlassian.net", ""),
            field("email", "Account email", false, true, "", ""),
            field("api_token", "API token", true, true, "Atlassian API token.", ""),
        ],
        instructions: sv![
            "One click connects via Atlassian sign-in in your browser (recommended).",
            "Manual: create an Atlassian API token and paste your site URL, account email, and token below."
        ],
        brand_color: "#0052cc".into(),
        logo: "jira".into(),
        aliases: sv!["issues", "tickets", "atlassian", "project management"],
        mcp_url: "https://mcp.atlassian.com/v1/mcp".into(),
        about: "".into(),
        access: sv!["Reads and searches issues your account can see.", "Creates, updates, and transitions issues; comments as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "monday".into(),
        title: "monday.com".into(),
        icon: "▦".into(),
        blurb: "Read boards and items, track work, create items and post updates.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![
            "One click connects via monday.com sign-in in your browser.",
            "Sign-in is fully local — tokens stay on this computer."
        ],
        brand_color: "#6161ff".into(),
        logo: "monday".into(),
        aliases: sv!["project management", "tasks", "boards", "work management"],
        mcp_url: "https://mcp.monday.com/mcp".into(),
        about: "Work with your monday.com boards — read items, summarize and aggregate board data, create items, and post updates. One-click sign-in runs entirely on this computer against monday.com's own agent service; agents get a small curated set of its tools, never the full catalog.".into(),
        access: sv!["Reads boards, items, and updates your account can see.", "Creates items, changes item values, and posts updates as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "confluence".into(),
        title: "Confluence".into(),
        icon: "◫".into(),
        blurb: "Search spaces, read pages, and draft documentation.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("base_url", "Atlassian site URL", false, true, "Example: https://example.atlassian.net", ""),
            field("email", "Account email", false, true, "", ""),
            field("api_token", "API token", true, true, "Atlassian API token.", ""),
        ],
        instructions: sv![
            "Create an Atlassian API token for your account.",
            "Paste your site URL, account email, and API token below."
        ],
        brand_color: "#172b4d".into(),
        logo: "confluence".into(),
        about: "".into(),
        access: sv!["Reads and searches spaces and pages your account can see.", "Creates pages as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "zendesk".into(),
        title: "Zendesk".into(),
        icon: "◇".into(),
        blurb: "Search tickets, summarize customer context, and draft replies.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("subdomain", "Zendesk subdomain", false, true, "For example, 'acme' for acme.zendesk.com.", ""),
            field("email", "Agent email", false, true, "", ""),
            field("api_token", "API token", true, true, "", ""),
        ],
        instructions: sv![
            "Create a Zendesk API token.",
            "Paste your subdomain, agent email, and API token below."
        ],
        brand_color: "#03363d".into(),
        logo: "zendesk".into(),
        about: "".into(),
        access: sv!["Reads and searches tickets your agent account can see.", "Creates tickets as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "linear".into(),
        title: "Linear".into(),
        icon: "⟋".into(),
        blurb: "Search, read, and create Linear issues.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_key", "API key", true, true, "Personal API key from Linear settings.", "lin_api_…"),
        ],
        instructions: sv![
            "In Linear, open Settings → Security & access → Personal API keys.",
            "Create a key and paste it below."
        ],
        brand_color: "#5e6ad2".into(),
        logo: "linear".into(),
        about: "".into(),
        access: sv!["Reads and searches issues your account can see.", "Creates issues as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "gitlab".into(),
        title: "GitLab".into(),
        icon: "▲".into(),
        blurb: "Work with issues and merge requests on GitLab.com or self-hosted.".into(),
        auth: "token".into(),
        two_way: false,
        fields: vec![
            field("base_url", "GitLab URL", false, false, "Leave empty for gitlab.com.", "https://gitlab.example.com"),
            field("token", "Personal access token", true, true, "Token with read_api scope (api for write actions).", "glpat-…"),
        ],
        instructions: sv![
            "Create a GitLab personal access token with the read_api scope (api for write actions).",
            "For self-hosted GitLab, enter your instance URL; leave empty for gitlab.com."
        ],
        brand_color: "#fc6d26".into(),
        logo: "gitlab".into(),
        about: "".into(),
        access: sv!["Reads issues and merge requests within your token's scope.", "Creates issues (needs the api scope; read_api stays read-only)."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "discord".into(),
        title: "Discord".into(),
        icon: "✦".into(),
        blurb: "Read channels and send messages through a Discord bot.".into(),
        auth: "bot_token".into(),
        two_way: false,
        fields: vec![
            field("bot_token", "Bot token", true, true, "From the Bot tab of your Discord application.", ""),
        ],
        instructions: sv![
            "Go to discord.com/developers/applications → New Application → Bot.",
            "Copy the bot token and paste it below.",
            "Use the OAuth2 URL generator to invite the bot to your server with Read/Send Messages permissions."
        ],
        brand_color: "#5865f2".into(),
        logo: "discord".into(),
        about: "".into(),
        access: sv!["Reads channels the bot can see.", "Sends messages as the bot."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "stripe".into(),
        title: "Stripe".into(),
        icon: "≋".into(),
        blurb: "Read-only access to customers, charges, and invoices.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_key", "Restricted API key", true, true, "Read-only restricted key recommended.", "rk_live_…"),
        ],
        instructions: sv![
            "In the Stripe Dashboard, create a restricted API key with read access to Customers, Charges, and Invoices.",
            "Paste the key below. The connector only exposes read tools."
        ],
        brand_color: "#635bff".into(),
        logo: "stripe".into(),
        about: "".into(),
        access: sv!["Reads customers, charges, and invoices — read-only.", "A restricted read-only key means write access isn't even possible."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "asana".into(),
        title: "Asana".into(),
        icon: "⊙".into(),
        blurb: "Search and read tasks and projects; create, update, and comment.".into(),
        auth: "token".into(),
        two_way: false,
        fields: vec![
            field("token", "Personal access token", true, true, "From the Asana developer console.", ""),
        ],
        instructions: sv![
            "In Asana, open My Settings → Apps → Manage developer apps.",
            "Create a personal access token and paste it below."
        ],
        brand_color: "#f06a6a".into(),
        logo: "asana".into(),
        aliases: sv!["project management", "tasks", "work management"],
        about: "Keep up with your Asana work — search and read tasks and projects, create tasks, and comment. Connects with a personal access token from the Asana developer console.".into(),
        access: sv!["Reads and searches tasks your account can see.", "Creates tasks as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "hubspot".into(),
        title: "HubSpot".into(),
        icon: "⊚".into(),
        blurb: "Search CRM records; log notes and tasks, update records. No deletes.".into(),
        auth: "token".into(),
        two_way: false,
        fields: vec![
            field("token", "Private app token", true, true, "Access token of a HubSpot private app.", "pat-…"),
        ],
        instructions: sv![
            "In HubSpot, go to Settings → Integrations → Private Apps and create an app.",
            "Grant CRM object read scopes (add the .write scopes for notes, tasks, and updates).",
            "Copy the access token and paste it below."
        ],
        brand_color: "#ff7a59".into(),
        logo: "hubspot".into(),
        managed: true,
        about: "Search and read your CRM; optionally log notes and tasks and update records. Read-only vs read & write is chosen at consent time, and chosen properties can be hidden from agents entirely.".into(),
        access: sv!["Reads contacts, companies, deals, and tickets.", "Read & write adds: log notes and tasks, update records, create contacts — never delete.", "Properties you hide are stripped before an agent ever sees a record."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "dropbox".into(),
        title: "Dropbox".into(),
        icon: "▣".into(),
        blurb: "Search, browse, and read files in Dropbox.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Dropbox token with files.metadata.read and files.content.read scopes.", ""),
        ],
        instructions: sv![
            "Create an app in the Dropbox App Console with files.metadata.read and files.content.read scopes.",
            "Generate an access token and paste it below. Managed sign-in will replace this manual step later."
        ],
        brand_color: "#0061ff".into(),
        logo: "dropbox".into(),
        about: "".into(),
        access: sv!["Reads file names and contents — read-only."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "box".into(),
        title: "Box".into(),
        icon: "▢".into(),
        blurb: "Search, browse, and read files in Box.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Box developer token or OAuth access token.", ""),
        ],
        instructions: sv![
            "Create a Box app at app.box.com/developers/console.",
            "Generate a developer token (or OAuth access token) and paste it below. Managed sign-in will replace this manual step later."
        ],
        brand_color: "#0061d5".into(),
        logo: "box".into(),
        about: "".into(),
        access: sv!["Reads file names and contents — read-only."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "whatsapp".into(),
        title: "WhatsApp".into(),
        icon: "◌".into(),
        blurb: "Send WhatsApp messages through Meta's official Cloud API (outbound only).".into(),
        auth: "token".into(),
        two_way: false,
        fields: vec![
            field("access_token", "Access token", true, true, "From your Meta app's WhatsApp setup page (a system-user token for long-lived access).", ""),
            field("phone_number_id", "Phone number ID", false, true, "The Cloud API phone number ID (not the phone number itself).", ""),
        ],
        instructions: sv![
            "Create a Meta app at developers.facebook.com and add the WhatsApp product.",
            "Copy the access token and the phone number ID from the API setup page.",
            "The free test number can message up to 5 verified recipients without business verification.",
            "Free-form messages only reach people who messaged your number in the last 24 hours; outside that window only approved templates are delivered."
        ],
        brand_color: "#25d366".into(),
        logo: "whatsapp".into(),
        about: "".into(),
        access: sv!["Sends messages from your Cloud API number.", "Outbound only — it cannot read your chats."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "quickbooks".into(),
        title: "QuickBooks".into(),
        icon: "◴".into(),
        blurb: "Read-only access to customers, invoices, and financial reports.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Intuit OAuth token with the com.intuit.quickbooks.accounting scope. Expires hourly.", ""),
            field("realm_id", "Company ID (realm ID)", false, true, "Shown during OAuth authorization and in the developer playground.", ""),
            field("environment", "Environment", false, false, "production (default) or sandbox.", "production"),
        ],
        instructions: sv![
            "Create an app at developer.intuit.com and authorize it against your company (the OAuth playground works for testing).",
            "Copy the access token and the company ID (realm ID) and paste them below.",
            "Intuit access tokens expire after about an hour. Managed sign-in will replace this manual step later."
        ],
        brand_color: "#2ca01c".into(),
        logo: "quickbooks".into(),
        about: "".into(),
        access: sv!["Reads customers, invoices, and reports — read-only."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "datadog".into(),
        title: "Datadog".into(),
        icon: "◍".into(),
        blurb: "Pull firing alerts, monitors, and the incident timeline.".into(),
        auth: "none".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![],
        available: false,
        brand_color: "#632ca6".into(),
        logo: "datadog".into(),
        about: "".into(),
        access: sv!["Access is limited to what the credentials you provide allow."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "salesforce".into(),
        title: "Salesforce".into(),
        icon: "☁".into(),
        blurb: "Read and update cases, accounts, and opportunities in the CRM.".into(),
        auth: "none".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![],
        available: false,
        brand_color: "#00a1e0".into(),
        logo: "salesforce".into(),
        about: "".into(),
        access: sv!["Access is limited to what the credentials you provide allow."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "docusign".into(),
        title: "Docusign".into(),
        icon: "✍".into(),
        blurb: "Track agreements, check envelope status, and send documents for signature.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Access token from a Docusign app (JWT or authorization-code grant).", ""),
        ],
        instructions: sv![
            "Create an app in the Docusign developer console and complete an OAuth grant.",
            "Paste the access token below; the account and API base are discovered automatically."
        ],
        brand_color: "#4c00ff".into(),
        logo: "docusign".into(),
        about: "".into(),
        access: sv!["Reads envelopes and their signing status.", "Sends documents for signature as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "clickup".into(),
        title: "ClickUp".into(),
        icon: "⌃".into(),
        blurb: "Search tasks and docs; create and update items.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_token", "Personal API token", true, true, "ClickUp → Settings → Apps → API Token.", "pk_…"),
        ],
        instructions: sv![
            "In ClickUp, open Settings → Apps and generate a personal API token.",
            "Paste it below."
        ],
        brand_color: "#7b68ee".into(),
        logo: "clickup".into(),
        about: "".into(),
        access: sv!["Reads and searches tasks and docs your account can see.", "Creates and updates tasks, and comments, as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "google_drive".into(),
        title: "Google Drive".into(),
        icon: "◬".into(),
        blurb: "Search, browse, and read files in Google Drive.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Google OAuth token with Drive read scopes.", ""),
        ],
        instructions: sv![
            "Use a Google OAuth access token with Drive readonly scope.",
            "Paste the access token below."
        ],
        brand_color: "#4285f4".into(),
        logo: "google_drive".into(),
        managed: true,
        managed_paused: true,
        account_field: "@identity".into(),
        about: "Search, browse, and read files across your Drive. Multiple accounts connect side by side.".into(),
        access: sv!["Reads and searches your files — read-only.", "Never edits or deletes anything in your Drive."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "canva".into(),
        title: "Canva".into(),
        icon: "◠".into(),
        blurb: "Browse, create, and export designs.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "OAuth access token", true, true, "Access token from a Canva Connect integration.", ""),
        ],
        instructions: sv![
            "Create a Connect integration at canva.com/developers and complete an OAuth grant.",
            "Paste the access token below."
        ],
        brand_color: "#00c4cc".into(),
        logo: "canva".into(),
        about: "".into(),
        access: sv!["Browses your designs and exports them — read-only."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "figma".into(),
        title: "Figma".into(),
        icon: "◐".into(),
        blurb: "Read design files and comments; export assets.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("access_token", "Personal access token", true, true, "Figma → Settings → Security → Personal access tokens.", "figd_…"),
        ],
        instructions: sv![
            "In Figma, open Settings → Security and generate a personal access token.",
            "Paste it below."
        ],
        brand_color: "#f24e1e".into(),
        logo: "figma".into(),
        about: "".into(),
        access: sv!["Reads design files and comments; exports assets.", "Comments as you — never edits a design."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "descript".into(),
        title: "Descript".into(),
        icon: "≣".into(),
        blurb: "Read and edit audio and video projects through their transcripts.".into(),
        auth: "none".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![],
        available: false,
        brand_color: "#0062ff".into(),
        logo: "descript".into(),
        about: "".into(),
        access: sv!["Access is limited to what the credentials you provide allow."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "clay".into(),
        title: "Clay".into(),
        icon: "⌒".into(),
        blurb: "Enrich people and companies; run outbound research workflows.".into(),
        auth: "none".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![],
        available: false,
        brand_color: "#1f2328".into(),
        logo: "clay".into(),
        about: "".into(),
        access: sv!["Access is limited to what the credentials you provide allow."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "close".into(),
        title: "Close".into(),
        icon: "❋".into(),
        blurb: "Read and update leads, contacts, and opportunities in the CRM.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_key", "API key", true, true, "Close → Settings → Developer → API Keys.", "api_…"),
        ],
        instructions: sv![
            "In Close, open Settings → Developer → API Keys and create a key.",
            "Paste it below."
        ],
        brand_color: "#276392".into(),
        logo: "close".into(),
        about: "".into(),
        access: sv!["Reads leads, contacts, and opportunities.", "Creates leads, updates opportunities, and logs notes as you."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "notion".into(),
        title: "Notion".into(),
        icon: "◰".into(),
        blurb: "Search pages, read content, query databases, create pages.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "Integration secret", true, true, "From an internal integration at notion.so/my-integrations; share the pages it should see with the integration.", "ntn_…"),
        ],
        instructions: sv![
            "One click connects via OpenWorker Cloud (recommended).",
            "Manual: create an internal integration at notion.so/my-integrations,",
            "copy its secret, and share the relevant pages with the integration."
        ],
        brand_color: "#1f2328".into(),
        logo: "notion".into(),
        managed: true,
        account_field: "account_id".into(),
        about: "Search and read the pages and databases you share with the connection, and create new pages. You choose exactly which pages it can see.".into(),
        access: sv!["Reads only the pages and databases shared with the connection.", "Creates pages — never edits or deletes existing ones."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "attio".into(),
        title: "Attio".into(),
        icon: "◵".into(),
        blurb: "Read your Attio CRM: objects, records, notes.".into(),
        auth: "oauth".into(),
        two_way: false,
        fields: vec![
            field("access_token", "API key", true, true, "Workspace Settings → Developers → API keys.", ""),
        ],
        instructions: sv![
            "One click connects via OpenWorker Cloud (recommended).",
            "Manual: create an API key under Workspace Settings → Developers."
        ],
        brand_color: "#2d7ff9".into(),
        logo: "attio".into(),
        managed: true,
        account_field: "account_id".into(),
        about: "Read your Attio CRM — objects, records, and lists — to prep meetings and answer pipeline questions, and log notes as you work.".into(),
        access: sv!["Reads objects, records, lists, and notes.", "Logs notes — records are never created or changed."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "posthog".into(),
        title: "PostHog".into(),
        icon: "◫".into(),
        blurb: "Query product analytics: events, funnels, saved insights.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("base_url", "PostHog URL", false, false, "Leave empty for US cloud; set for EU cloud or self-hosted.", "https://us.posthog.com"),
            field("api_key", "Personal API key", true, true, "Settings → Personal API keys (read access is enough).", "phx_…"),
            field("project_id", "Project ID", false, true, "Settings → Project → Project ID. Add more projects as extra accounts.", ""),
        ],
        instructions: sv![
            "In PostHog, open Settings → Personal API keys and create a key.",
            "Copy your Project ID from Settings → Project.",
            "One project per account — connect again to add another project."
        ],
        brand_color: "#f54e00".into(),
        logo: "posthog".into(),
        account_field: "project_id".into(),
        about: "".into(),
        access: sv!["Runs read-only queries on the connected project: events, funnels, insights."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "mixpanel".into(),
        title: "Mixpanel".into(),
        icon: "◭".into(),
        blurb: "Query Mixpanel events and segmentation.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("username", "Service account username", false, true, "", ""),
            field("secret", "Service account secret", true, true, "", ""),
            field("project_id", "Project ID", false, true, "Add more projects as extra accounts.", ""),
        ],
        instructions: sv![
            "In Mixpanel, open Organization Settings → Service Accounts and create one.",
            "Copy the username, the secret, and your Project ID (Project Settings)."
        ],
        brand_color: "#7856ff".into(),
        logo: "mixpanel".into(),
        account_field: "project_id".into(),
        about: "".into(),
        access: sv!["Runs read-only queries on the connected project."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "amplitude".into(),
        title: "Amplitude".into(),
        icon: "∿".into(),
        blurb: "Query Amplitude charts data: active users, event totals.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_key", "API key", true, true, "Project Settings → API Keys.", ""),
            field("secret_key", "Secret key", true, true, "", ""),
        ],
        instructions: sv![
            "In Amplitude, open Settings → Projects → your project → API Keys.",
            "Copy the API key and secret key. One project per account."
        ],
        brand_color: "#1e61f0".into(),
        logo: "amplitude".into(),
        account_field: "@identity".into(),
        about: "".into(),
        access: sv!["Runs read-only chart queries: active users, event totals."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "apollo".into(),
        title: "Apollo.io".into(),
        icon: "☄".into(),
        blurb: "Enrich people and companies; search the B2B database.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_key", "API key", true, true, "Settings → Integrations → API.", ""),
            field("label", "Account label", false, false, "Name this account (used if you connect more than one).", "work"),
        ],
        instructions: sv![
            "In Apollo, open Settings → Integrations → API and create an API key.",
            "Enrichment and search endpoints require a paid Apollo plan."
        ],
        brand_color: "#fbbf24".into(),
        logo: "apollo".into(),
        account_field: "@identity".into(),
        about: "".into(),
        access: sv!["Searches and enriches people and companies, using your Apollo credits."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "hunter".into(),
        title: "Hunter".into(),
        icon: "✉".into(),
        blurb: "Find and verify professional email addresses by domain.".into(),
        auth: "api_token".into(),
        two_way: false,
        fields: vec![
            field("api_key", "API key", true, true, "hunter.io → API → API keys.", ""),
        ],
        instructions: sv![
            "In Hunter, open API → API keys and copy your key."
        ],
        brand_color: "#fa5320".into(),
        logo: "hunter".into(),
        account_field: "@identity".into(),
        about: "".into(),
        access: sv!["Finds and verifies email addresses, using your Hunter quota."],
        ..Default::default()
    },
    ConnectorDescriptor {
        name: "pagerduty".into(),
        title: "PagerDuty".into(),
        icon: "◔".into(),
        blurb: "See who's on-call and review active incidents before paging.".into(),
        auth: "none".into(),
        two_way: false,
        fields: vec![],
        instructions: sv![],
        available: false,
        brand_color: "#06ac38".into(),
        logo: "pagerduty".into(),
        about: "".into(),
        access: sv!["Access is limited to what the credentials you provide allow."],
        ..Default::default()
    },
    ]
}

static DESCRIPTOR_NAMES: &[&str] = &[
    "telegram",
    "slack",
    "email",
    "gmail",
    "google_calendar",
    "browser",
    "github",
    "outlook",
    "jira",
    "monday",
    "confluence",
    "zendesk",
    "linear",
    "gitlab",
    "discord",
    "stripe",
    "asana",
    "hubspot",
    "dropbox",
    "box",
    "whatsapp",
    "quickbooks",
    "datadog",
    "salesforce",
    "docusign",
    "clickup",
    "google_drive",
    "canva",
    "figma",
    "descript",
    "clay",
    "close",
    "notion",
    "attio",
    "posthog",
    "mixpanel",
    "amplitude",
    "apollo",
    "hunter",
    "pagerduty",
];

/// All connector descriptors (including placeholders with `available = false`).
pub fn all_descriptors() -> Vec<ConnectorDescriptor> {
    build_descriptors()
}

/// Lookup a connector descriptor by canonical id (= `name`).
pub fn get_descriptor(name: &str) -> Option<ConnectorDescriptor> {
    build_descriptors()
        .into_iter()
        .find(|d| d.name == name)
}

/// Canonical connector ids — stable list for tests.
pub fn descriptor_names() -> Vec<&'static str> {
    DESCRIPTOR_NAMES.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot of Python `list_descriptors()` names (2026-09-12).
    const PYTHON_NAMES: &[&str] = &[
        "amplitude",
        "apollo",
        "asana",
        "attio",
        "box",
        "browser",
        "canva",
        "clay",
        "clickup",
        "close",
        "confluence",
        "datadog",
        "descript",
        "discord",
        "docusign",
        "dropbox",
        "email",
        "figma",
        "github",
        "gitlab",
        "gmail",
        "google_calendar",
        "google_drive",
        "hubspot",
        "hunter",
        "jira",
        "linear",
        "mixpanel",
        "monday",
        "notion",
        "outlook",
        "pagerduty",
        "posthog",
        "quickbooks",
        "salesforce",
        "slack",
        "stripe",
        "telegram",
        "whatsapp",
        "zendesk",
    ];

    #[test]
    fn catalog_names_match_python_snapshot() {
        let mut rust: Vec<&str> = descriptor_names();
        rust.sort();
        let mut py = PYTHON_NAMES.to_vec();
        py.sort();
        assert_eq!(rust, py, "Rust catalog names must match Python DESCRIPTORS");
    }

    #[test]
    fn google_calendar_id_uses_underscore() {
        assert!(get_descriptor("google_calendar").is_some());
        assert!(get_descriptor("google-calendar").is_none());
        let d = get_descriptor("google_calendar").unwrap();
        assert!(d.managed);
        assert!(d.managed_paused);
    }

    #[test]
    fn placeholders_unavailable() {
        for name in ["datadog", "salesforce", "descript", "clay", "pagerduty"] {
            let d = get_descriptor(name).expect(name);
            assert!(!d.available, "{name} should be available=false");
        }
    }

    #[test]
    fn mcp_backed_have_url() {
        for name in ["jira", "monday"] {
            let d = get_descriptor(name).expect(name);
            assert!(!d.mcp_url.is_empty(), "{name} needs mcp_url");
            assert!(matches!(auth_kind(&d), AuthKind::McpOAuth { .. }));
        }
    }

    #[test]
    fn managed_provider_map() {
        assert!(matches!(
            auth_kind(&get_descriptor("gmail").unwrap()),
            AuthKind::ManagedOAuth { provider: "google" }
        ));
        assert!(matches!(
            auth_kind(&get_descriptor("outlook").unwrap()),
            AuthKind::ManagedOAuth { provider: "microsoft" }
        ));
        assert!(matches!(
            auth_kind(&get_descriptor("slack").unwrap()),
            AuthKind::ManagedOAuth { provider: "slack" }
        ));
    }

    #[test]
    fn catalog_count_is_forty() {
        assert_eq!(all_descriptors().len(), 40);
        assert_eq!(descriptor_names().len(), 40);
    }
}
