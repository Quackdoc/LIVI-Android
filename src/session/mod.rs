// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! How a phone reaches the Android Auto stack: the in-process helper it asks for sessions, and the USB
//! side that finds wired phones.

pub mod helper;
pub mod usb;
