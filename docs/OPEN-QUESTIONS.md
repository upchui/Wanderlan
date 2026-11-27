# Open questions

Points that were deliberately not decided during the implementation and
need a decision by the project owner.

1. **IPv6 in the transport network.** Site addresses are IPv4 only (VXLAN
   over IPv4, path MTU probes and sync over IPv4). IPv6 would need VXLAN over
   IPv6, a different overhead in the MSS formula (70 instead of 50 bytes of
   outer headers) and IPv6 addresses in `network.toml`. Traffic *inside* the stretched
   VLANs may of course be IPv6 (it is layer 2); its TCP MSS is clamped too.
2. **Configurable interface names** for hosts that already use `br0`, `vx*`
   or `v<tag>*` (the tunnels of further networks) for something else.
3. **Trust model of the sync.** Who may change what is enforced by the
   daemons among themselves (who changes a client's tunnel, clients only
   change themselves and their own clients, servers check the changes of
   their clients), and clients only receive their part of the configuration.
   Neighbours prove their identity with keys derived from it (probes and
   sync); invitation codes are valid for a day and travel in clear text in
   the transport network (VPN), which is assumed to be trusted. A change is
   authenticated hop by hop, not signed by the site that made it: a server
   takes what another server sends, so someone with root on a server can
   change anything. Should changes be signed by their author?
4. **License.** `Cargo.toml` declares `MIT OR Apache-2.0` as a placeholder;
   the license files are still missing. Which license should apply?
5. **Duplicate MAC addresses.** The loop guard cannot tell a real loop from
   two hosts with the same MAC address at different sites; both shut down a
   tunnel port. Is that acceptable or should the guard only warn first?
