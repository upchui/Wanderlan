# Open questions

Points that were deliberately not decided during the implementation and
need a decision by the project owner.

1. **IPv6 in the transport network.** Site addresses are IPv4 only (GRETAP
   over IPv4, path MTU probes and sync over IPv4). IPv6 would need ip6gretap,
   different overhead in the MSS formula (40 instead of 20 bytes outer
   header) and IPv6 addresses in `mesh.toml`. Traffic *inside* the stretched
   VLANs may of course be IPv6 (it is layer 2); its TCP MSS is clamped too.
2. **Backup hub for spokes with automatic failover.** A spoke has exactly one
   hub. A second hub with automatic switching needs a rule that guarantees
   that a spoke is connected to at most one hub at any time (otherwise
   loops), e.g. health checks plus a delay, and a way to express it in
   `mesh.toml` (`hub = ["a", "b"]` with priorities?). The topology check and
   the sync are prepared for it (a moved spoke can still fetch the new
   configuration from its former hub for 5 minutes).
3. **Configurable interface names** for hosts that already use `br0` or
   `gt-*` for something else.
4. **Trust model of the sync.** Every site may change the mesh of all sites;
   the transport network (VPN) is assumed to be trusted and the control
   channel accepts only the addresses of direct peers, but messages are not
   signed. Should changes be signed (e.g. per-site keys or a shared secret),
   or restricted to hubs?
5. **License.** `Cargo.toml` declares `MIT OR Apache-2.0` as a placeholder;
   the license files are still missing. Which license should apply?
6. **Duplicate MAC addresses.** The loop guard cannot tell a real loop from
   two hosts with the same MAC address at different sites; both shut down a
   tunnel port. Is that acceptable or should the guard only warn first?
