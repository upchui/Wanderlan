# Open questions

Points that were deliberately not decided during the implementation and
need a decision by the project owner.

1. **IPv6 in the transport network.** Site addresses are IPv4 only (GRETAP
   over IPv4, path MTU probes and sync over IPv4). IPv6 would need ip6gretap,
   different overhead in the MSS formula (40 instead of 20 bytes outer
   header) and IPv6 addresses in `mesh.toml`. Traffic *inside* the stretched
   VLANs may of course be IPv6 (it is layer 2); its TCP MSS is clamped too.
2. **Configurable interface names** for hosts that already use `br0`,
   `wl-*` or `gt-*` for something else.
3. **Trust model of the sync.** Who may change what is enforced by the
   daemons among themselves (managers, restricted sites, clients only change
   themselves and their own clients, servers check the changes of their
   clients), and clients only receive their part of the configuration. The
   transport network (VPN) is assumed to be trusted and the control channel
   accepts only the addresses of tunnel neighbours (and join requests with a
   one-time invitation code), but messages are not signed and codes travel
   in clear text: someone with root on a server could change anything.
   Should changes be signed (e.g. per-site keys)?
4. **License.** `Cargo.toml` declares `MIT OR Apache-2.0` as a placeholder;
   the license files are still missing. Which license should apply?
5. **Duplicate MAC addresses.** The loop guard cannot tell a real loop from
   two hosts with the same MAC address at different sites; both shut down a
   tunnel port. Is that acceptable or should the guard only warn first?
