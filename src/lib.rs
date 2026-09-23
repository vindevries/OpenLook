//! OpenLook — an Outlook-style native mail client for Linux.
//!
//! The UI reads exclusively from a local SQLite cache ([`db`]), which the
//! background [`sync`] engine keeps in step with Microsoft Graph. That split
//! is what lets the app start instantly, read mail with no network, and
//! queue changes made offline until it can reach the server again.

pub mod app;
pub mod auth;
pub mod config;
pub mod connector;
pub mod db;
pub mod demo;
pub mod graph;
pub mod hubspot;
pub mod invite;
pub mod model;
pub mod plugin;
pub mod sync;
pub mod ui;
pub mod util;
