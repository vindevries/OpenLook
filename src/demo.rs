//! Demo mailbox, seeded into the local database so the whole app — including
//! offline behaviour — works before anyone signs in.

use anyhow::Result;
use chrono::{Duration, Utc};

use crate::db::Db;
use crate::graph::folder_rank;
use crate::model::{Address, Body, Folder, MessageSummary, Pending};
use crate::util::html_to_text;

fn ago(hours: i64) -> String {
    (Utc::now() - Duration::hours(hours)).to_rfc3339()
}

fn html(paragraphs: &[&str]) -> String {
    paragraphs.iter().map(|p| format!("<p>{p}</p>")).collect::<Vec<_>>().join("\n")
}

struct Seed {
    folder: &'static str,
    from: (&'static str, &'static str),
    subject: &'static str,
    paragraphs: &'static [&'static str],
    age_hours: i64,
    unread: bool,
    to: &'static [(&'static str, &'static str)],
}

const OWNER: (&str, &str) = ("You", "you@example.com");

pub fn seed(db: &Db) -> Result<()> {
    let folders = [
        "Inbox",
        "Drafts",
        "Sent Items",
        "Deleted Items",
        "Junk Email",
        "Archive",
    ];
    let folder_rows: Vec<Folder> = folders
        .iter()
        .map(|name| Folder {
            id: name.to_lowercase().replace(' ', "_"),
            display_name: (*name).to_string(),
            unread_count: 0,
            total_count: 0,
        })
        .collect();
    // Keep Outlook's ordering even though these are synthetic folders.
    let mut folder_rows = folder_rows;
    folder_rows.sort_by_key(|f| folder_rank(&f.display_name));
    db.upsert_folders(&folder_rows)?;

    let seeds: &[Seed] = &[
        Seed {
            folder: "inbox",
            from: ("Anna Visser", "anna.visser@contoso.com"),
            subject: "Q3 planning review — agenda",
            paragraphs: &[
                "Hi,",
                "Below is the agenda for Thursday's Q3 planning review. Please have your \
                 team's capacity numbers ready before the meeting.",
                "1. Roadmap status — 2. Hiring plan — 3. Budget checkpoint — 4. AOB",
                "Thanks,<br>Anna",
            ],
            age_hours: 2,
            unread: true,
            to: &[OWNER],
        },
        Seed {
            folder: "inbox",
            from: ("GitLab", "noreply@gitlab.com"),
            subject: "Pipeline #48211 passed on main",
            paragraphs: &[
                "Your pipeline for <b>infrastructure/monitoring</b> on branch <b>main</b> \
                 passed in 6m 12s.",
                "Stages: build ✓ &nbsp; test ✓ &nbsp; deploy ✓",
            ],
            age_hours: 4,
            unread: true,
            to: &[OWNER],
        },
        Seed {
            folder: "inbox",
            from: ("Mark de Jong", "mark@fabrikam.nl"),
            subject: "RE: SCOM connector license renewal",
            paragraphs: &[
                "Vincent,",
                "Thanks for the quote. Procurement has approved the renewal for another \
                 12 months. Can you send the invoice to invoices@fabrikam.nl with PO \
                 number 2026-0834?",
                "Best regards,<br>Mark",
            ],
            age_hours: 7,
            unread: true,
            to: &[OWNER],
        },
        Seed {
            folder: "inbox",
            from: ("Microsoft 365 Message Center", "o365mc@microsoft.com"),
            subject: "Weekly digest: Microsoft 365 changes",
            paragraphs: &[
                "Here's your weekly summary of changes across Microsoft 365.",
                "<b>MC912345</b>: Outlook — new calendar sharing controls rolling out in \
                 September.",
                "<b>MC912822</b>: Teams — updated meeting recap experience.",
            ],
            age_hours: 20,
            unread: false,
            to: &[OWNER],
        },
        Seed {
            folder: "inbox",
            from: ("Sofia Lindqvist", "sofia@northwind.se"),
            subject: "Workshop follow-up and next steps",
            paragraphs: &[
                "Hi Vincent,",
                "Great session yesterday. As discussed, we'll set up the pilot environment \
                 next week. I've shared the checklist document with your team.",
                "Could you confirm who from your side will own the firewall change request?",
                "Cheers,<br>Sofia",
            ],
            age_hours: 26,
            unread: false,
            to: &[OWNER],
        },
        Seed {
            folder: "inbox",
            from: ("Jira", "jira@atlassian.net"),
            subject: "[OPS-1142] Deployment runbook needs review",
            paragraphs: &[
                "Anna Visser assigned <b>OPS-1142</b> to you.",
                "“Deployment runbook needs review before the October release. Please \
                 update the rollback section.”",
            ],
            age_hours: 30,
            unread: false,
            to: &[OWNER],
        },
        Seed {
            folder: "inbox",
            from: ("Thomas Berg", "thomas.berg@adventure-works.com"),
            subject: "Coffee next week?",
            paragraphs: &[
                "Hey Vincent,",
                "I'm in Amsterdam Tuesday and Wednesday. Would be good to catch up — are \
                 you free for a coffee near Zuidas either morning?",
                "Thomas",
            ],
            age_hours: 48,
            unread: false,
            to: &[OWNER],
        },
        Seed {
            folder: "sent_items",
            from: OWNER,
            subject: "RE: Workshop follow-up and next steps",
            paragraphs: &[
                "Hi Sofia,",
                "Thanks — the checklist looks complete. Jeroen will own the firewall \
                 change request; I've cc'd him on the ticket.",
                "Vincent",
            ],
            age_hours: 24,
            unread: false,
            to: &[("Sofia Lindqvist", "sofia@northwind.se")],
        },
        Seed {
            folder: "sent_items",
            from: OWNER,
            subject: "SCOM connector license renewal — quote",
            paragraphs: &[
                "Hi Mark,",
                "As requested, here's the renewal quote for the SCOM connector, valid \
                 until the end of the month. Let me know if procurement needs anything else.",
                "Vincent",
            ],
            age_hours: 50,
            unread: false,
            to: &[("Mark de Jong", "mark@fabrikam.nl")],
        },
        Seed {
            folder: "junk_email",
            from: ("Prize Department", "winner@lottery-example.biz"),
            subject: "You have been selected!!!",
            paragraphs: &[
                "Congratulations! You have been selected to receive a prize. Click here \
                 to claim it now.",
            ],
            age_hours: 12,
            unread: false,
            to: &[OWNER],
        },
        Seed {
            folder: "archive",
            from: ("HR Team", "hr@opslogix.com"),
            subject: "Summer party photos",
            paragraphs: &[
                "The photos from the summer party are now available on the intranet. \
                 Thanks everyone for a great evening!",
            ],
            age_hours: 400,
            unread: false,
            to: &[OWNER],
        },
    ];

    for (index, seed) in seeds.iter().enumerate() {
        let body_html = html(seed.paragraphs);
        let summary = MessageSummary {
            id: format!("demo-{index}"),
            folder_id: seed.folder.to_string(),
            subject: seed.subject.to_string(),
            from: Address::new(seed.from.0, seed.from.1),
            received: ago(seed.age_hours),
            preview: html_to_text(&body_html).chars().take(140).collect(),
            is_read: !seed.unread,
            has_attachments: false,
            pending: Pending::None,
        };
        let to: Vec<Address> = seed.to.iter().map(|(n, a)| Address::new(*n, *a)).collect();
        let body = Body { is_html: true, content: body_html };
        db.insert_local_message(&summary, &to, &[], &body)?;
    }
    db.recompute_counts()?;
    Ok(())
}
