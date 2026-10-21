//! The single event loop shared by backtest and live.
//!
//! Per event: the broker settles pending orders against the new event
//! *before* the strategy runs, so an order decided on event t fills no
//! earlier than t+1. Only the provider differs between backtest and live.
