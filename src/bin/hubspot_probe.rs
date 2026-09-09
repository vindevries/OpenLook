//! Read-only inspector for a HubSpot portal.
//!
//! It answers the questions that decide how OpenLook should model tickets:
//! which pipeline stages exist, how many tickets came from the connected
//! inbox versus a web form, whether tickets map 1:1 to threads, and what a
//! thread message actually looks like.
//!
//!   cargo run --bin hubspot_probe            # structure and counts only
//!   cargo run --bin hubspot_probe -- --sample # plus one redacted example
//!
//! It only ever reads. Nothing is sent, created or modified. Output is
//! deliberately structural: subjects are truncated and addresses masked,
//! so a probe can be pasted into a conversation without leaking customer
//! details. Token is read from ~/.config/openlook/hubspot-token.

use std::collections::HashMap;

use openlook::hubspot::HubSpot;

fn mask(email: &str) -> String {
    match email.split_once('@') {
        Some((user, domain)) => {
            let head = user.chars().take(2).collect::<String>();
            format!("{head}***@{domain}")
        }
        None if email.is_empty() => "(none)".into(),
        None => "***".into(),
    }
}

fn clip(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > limit {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    } else {
        flat
    }
}

#[tokio::main]
async fn main() {
    let sample = std::env::args().any(|a| a == "--sample");
    let path = openlook::config::config_dir().join("hubspot-token");
    let token = match std::fs::read_to_string(&path) {
        Ok(token) => token.trim().to_string(),
        Err(e) => {
            eprintln!("no token at {}: {e}", path.display());
            eprintln!("create a private app (Settings → Integrations → Private Apps) with");
            eprintln!("scopes: tickets, crm.objects.contacts.read, conversations.read");
            eprintln!("then: umask 077 && printf '%s' '<token>' > {}", path.display());
            std::process::exit(1);
        }
    };

    let http = reqwest::Client::builder()
        .user_agent("OpenLook-probe")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client");
    let hs = HubSpot::new(http, token);

    println!("== pipeline stages (these would become folders) ==");
    match hs.stages().await {
        Ok(stages) => {
            for stage in &stages {
                println!("  {:<28} {}", stage.label, if stage.closed { "(closed)" } else { "" });
            }
            println!("  {} stages", stages.len());
        }
        Err(e) => println!("  FAILED: {e}"),
    }

    println!("\n== tickets ==");
    let tickets = match hs.tickets(50).await {
        Ok(tickets) => tickets,
        Err(e) => {
            println!("  FAILED: {e}");
            return;
        }
    };
    println!("  {} fetched", tickets.len());
    let with_content = tickets.iter().filter(|t| !t.content.trim().is_empty()).count();
    println!("  {with_content} have a description (web-form origin)");
    println!("  {} have an associated contact", tickets.iter().filter(|t| !t.contact_ids.is_empty()).count());

    println!("\n== do tickets map 1:1 to conversation threads? ==");
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let mut example_thread: Option<String> = None;
    for ticket in tickets.iter().take(15) {
        match hs.ticket_threads(&ticket.id).await {
            Ok(threads) => {
                *counts.entry(threads.len()).or_default() += 1;
                if example_thread.is_none() {
                    example_thread = threads.first().cloned();
                }
            }
            Err(e) => {
                println!("  association lookup FAILED: {e}");
                break;
            }
        }
    }
    let mut summary: Vec<_> = counts.into_iter().collect();
    summary.sort();
    for (threads, tickets) in summary {
        println!("  {tickets} ticket(s) with {threads} thread(s)");
    }

    println!("\n== a thread's messages ==");
    match example_thread {
        None => println!("  no thread found on the sampled tickets"),
        Some(thread_id) => match hs.thread_messages(&thread_id).await {
            Ok(messages) => {
                println!("  {} messages", messages.len());
                for m in messages.iter().take(4) {
                    println!(
                        "   {:<8} {:<24} html={:<5} attachments={}",
                        if m.outgoing { "sent" } else { "received" },
                        mask(&m.sender_email),
                        m.is_html,
                        m.attachments.len()
                    );
                }
                if sample {
                    if let Some(m) = messages.first() {
                        println!("\n  sample body (clipped): {}", clip(&m.body, 200));
                    }
                }
            }
            Err(e) => println!("  FAILED: {e}"),
        },
    }

    if sample {
        println!("\n== sample ticket (redacted) ==");
        if let Some(t) = tickets.first() {
            println!("  subject : {}", clip(&t.subject, 60));
            println!("  stage   : {}", t.stage_id);
            println!("  origin  : {}", if t.content.trim().is_empty() { "connected inbox" } else { "web form" });
            for id in t.contact_ids.iter().take(1) {
                if let Ok(contact) = hs.contact(id).await {
                    println!("  contact : {} <{}>", contact.name, mask(&contact.email));
                }
            }
        }
    }
    // Assemble a real ticket the way the reading pane will, and report its
    // shape only — never its text.
    println!("\n== assembling a real ticket for the reading pane ==");
    let mut assembled = false;
    for ticket in tickets.iter().take(15) {
        let Ok(threads) = hs.ticket_threads(&ticket.id).await else { continue };
        if threads.is_empty() {
            continue;
        }
        let mut collected = Vec::new();
        for thread in &threads {
            if let Ok(messages) = hs.thread_messages(thread).await {
                collected.push((thread.clone(), messages));
            }
        }
        let total: usize = collected.iter().map(|(_, m)| m.len()).sum();
        let html = openlook::hubspot::assemble_ticket_html("", &collected);
        println!("  ticket with {} thread(s), {total} message(s)", collected.len());
        println!("    document: {} bytes", html.len());
        println!("    message blocks rendered: {}", html.matches("ol-block").count());
        println!("    thread labels: {}", html.matches("ol-thread").count());
        println!("    outgoing/incoming markers: {}/{}", html.matches("&rarr;").count(), html.matches("&larr;").count());
        assembled = true;
        break;
    }
    if !assembled {
        println!("  no ticket with a thread found in the sample");
    }

    println!("\nread-only probe finished; nothing was sent or modified.");
}
