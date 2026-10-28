//! The nucleos daemon.
//!
//! Three jobs, and the boundary is the point:
//!
//! - **Authentication.** Turn a connection into a `Principal{tenant, subject}`. The kernel
//!   never verifies a signature; it receives the principal as data and hands it to policy.
//! - **Translation.** Convert wire types into kernel types and events back. Every value
//!   that exists only because the wire needs it stops here.
//! - **Transport.** The listener, TLS material, and connection limits. None of these are
//!   hot-reloadable — see `docs/pitfalls.md` §12.
//!
//! The kernel itself is a library that knows none of the above.

fn main() {
    // P0: the skeleton builds and the boundary is visible. The listener lands in P2, once
    // the engine has something to serve.
    println!(
        "nucleosd {} (kernel {}, abi {})",
        env!("CARGO_PKG_VERSION"),
        nucleos::VERSION,
        nucleos_proto::ABI_VERSION,
    );
}
