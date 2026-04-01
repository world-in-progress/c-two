//! HTTP client for C-Two relay transport.
//!
//! Provides [`HttpClient`] for making CRM calls through an HTTP relay
//! server, and [`HttpClientPool`] for reference-counted connection
//! pooling.

mod client;
mod pool;

pub use client::{HttpClient, HttpError};
pub use pool::HttpClientPool;
