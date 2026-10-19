// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:MD service binary - market-data publication (ITCH/Mold/Soup) + LULD bands.
//!
//! Design: `design/sentin8-exchange8-md-technical-design.md` (see the
//! service doc for the exact file). Core logic in the sibling modules;
//! this shell wires the tokio HTTP/TLS surface via the platform
//! `sentin8-shell` crate (registration, DB pool, health).

#![allow(dead_code)]

// (master design §11.1). Workers (bus consumers, session
// supervisors, purge jobs) are in-process tokio tasks, so the
mod book;
mod calendar;
mod luld;
mod protection;
mod registration;
mod openapi;
mod snapshot;

use std::sync::Arc;

/// Service state: shell core + the MemBus publish handle (see
/// services/md/DESIGN.md §2 - MemBus publisher).
///
/// MD is a pub/sub-class publisher: `md.trade` (tape) and `md.book`
/// (deltas) - newest-wins families, next message supersedes.
pub struct MdState {
    pub core: sentin8::shell::ServiceCore,
    pub membus: sentin8::membus::ServiceBus,
}

fn extend_router(
    app: axum::Router<std::sync::Arc<MdState>>,
    _state: std::sync::Arc<MdState>,
) -> axum::Router<std::sync::Arc<MdState>> {
    // ITCH/Mold/Soup publishers are dedicated UDP/TCP tasks (design §4.3);
    // the HTTP router carries health + admin endpoints only.

    app
        // ── Sentin8:Policy admin API (see services/md/DESIGN.md §4 - shared implementation;
        // handlers generic over this state via the HasPolicyAdmin impl below;
        // axum 0.8 cannot merge a bound Router<()> into Router<std::sync::Arc<MdState>>).
        .route(
            "/v1/admin/policy-sets",
            axum::routing::get(sentin8::shield::admin::list_policy_sets::<std::sync::Arc<MdState>>),
        )
        .route(
            "/v1/admin/policy-sets/{name}",
            axum::routing::get(sentin8::shield::admin::get_policy_set::<std::sync::Arc<MdState>>)
                .put(sentin8::shield::admin::put_policy_set::<std::sync::Arc<MdState>>)
                .delete(sentin8::shield::admin::delete_policy_set::<std::sync::Arc<MdState>>),
        )
        .route(
            "/v1/admin/policy-sets/{name}/test",
            axum::routing::post(sentin8::shield::admin::test_policy_set::<std::sync::Arc<MdState>>),
        )
        .route(
            "/v1/admin/policy-sets/enforce",
            axum::routing::post(
                sentin8::shield::admin::admin_enforce_policy_set::<std::sync::Arc<MdState>>,
            ),
        )
        .route(
            "/v1/admin/policy-sets/shadow",
            axum::routing::post(
                sentin8::shield::admin::admin_shadow_policy_set::<std::sync::Arc<MdState>>,
            ),
        )
        .route(
            "/v1/admin/policy-sets/disable",
            axum::routing::post(
                sentin8::shield::admin::admin_disable_policy_set::<std::sync::Arc<MdState>>,
            ),
        )
        .route(
            "/v1/admin/relations",
            axum::routing::get(sentin8::shield::admin::list_relations::<std::sync::Arc<MdState>>)
                .put(sentin8::shield::admin::put_relation::<std::sync::Arc<MdState>>)
                .delete(sentin8::shield::admin::delete_relation::<std::sync::Arc<MdState>>),
        )
        .route(
            "/v1/admin/relations/check",
            axum::routing::get(sentin8::shield::admin::check_relation::<std::sync::Arc<MdState>>),
        )
        .route(
            "/v1/admin/attributes",
            axum::routing::get(sentin8::shield::admin::list_attributes::<std::sync::Arc<MdState>>)
                .put(sentin8::shield::admin::put_attribute::<std::sync::Arc<MdState>>)
                .delete(sentin8::shield::admin::delete_attribute::<std::sync::Arc<MdState>>),
        )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    sentin8::shell::serve(
        sentin8::shell::Sentin8Shell::new("exchange8:md", "EXCHANGE8_MD")
            .with_registration(registration::print)
            .with_state(|core| async move {
                // MD publishes the tape + book deltas
                // (pub/sub class) onto the Memory Bus.
                let membus =
                    sentin8::membus::ServiceBus::connect("exchange8:md", &core.config.valkey_url)
                        .await;
                Ok(Arc::new(MdState { core, membus }))
            })
            .with_router(extend_router),
    )
    .await
}

/// Shared policy-admin surface accessor (see services/md/DESIGN.md §4): the shell boot
/// loaded the engine into `core.admin`; the shared handlers extract it via
/// `HasPolicyAdmin` so the admin surface and the request path evaluate the
/// SAME cache. (MdState is bin-local, so the impl lives here.)
impl sentin8::shield::admin::HasPolicyAdmin for MdState {
    fn policy_admin(&self) -> &sentin8::shield::admin::SharedAdminState {
        &self.core.admin
    }
}
