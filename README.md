# brb_node_snfs

[MaidSafe website](http://maidsafe.net) | [Safe Network Forum](https://safenetforum.org/)
:-------------------------------------: | :---------------------------------------------:

## About

This crate is an attempt to build an async P2P distributed filesystem.  This is work-in-progress research and nowhere near ready for any real use.

Key components are:
* [sn_fs](https://github.com/maidsafe/sn_fs/) - Safe Network filesystem library
* [brb](https://github.com/maidsafe/brb/) - Byzantine reliable broadcast
* [qp2p](https://github.com/maidsafe/qp2p/) - Quic protocol P2P library

## Building

On ubuntu:

```
$ sudo apt install build-essential
$ sudo apt install libfuse-dev
$ sudo apt install pkg-config
$ cargo build
```

## Running

A node can be started in genesis (host) mode or client mode.

A genesis node will trust itself and establish a new BRB voting group with itself as the only peer.

A client node accepts the IP of a peer, trusts the peer, and requests to join its voting group.  The peer then sponsors the new client and proposes to the voting group that it be added to the group.  If all goes well, the proposal will be approved, and the client joins the group as a voting peer.

Also, an empty directory must be created to serve as a filesystem mount point.  We use `/tmp/safe` here.

To start a genesis node, behind NAT, with local IP 192.168.0.250 and public IP 207.45.36.125:

```
$ mkdir /tmp/safe
$ brb_node_snfs --local-ip 192.168.0.250 --local-port 12000 --external-ip 207.45.36.125 --external-port 12000 /tmp/safe``
```

Note that if local IP matches the public IP then --external-ip and --external-port can be omitted.

Port can be any number, but make sure your firewall is not blocking incoming UDP packets.

To start a client node, behind NAT, with local IP 192.168.0.251 and public IP 207.45.36.126 and connecting to (genesis) peer 207.45.36.125.

```
$ mkdir /tmp/safe
$ brb_node_snfs --local-ip 192.168.0.251 --local-port 12000 --external-ip 207.45.36.126
 --external-port 12000 /tmp/safe 207.45.36.125:12000
```

If all goes well, in a few seconds the client will connect to the host and exchange BRB messages.  After a few seconds, you should be able to issue the `peers` command on either side, and see that there are 2 peers, and both are considered voting members.  Eg:

```
> peers
i:541914@168.235.79.83:12000 (voting)
i:64c372@18.116.32.207:12000 (voting) (self)
```

Use the `help` command for a list of other available commands.

At this point, you can create a file in /tmp/safe and verify that it appears on both nodes.  Also directories, symlinks, etc.  Just use regular filesystem commands, eg:

```
$ echo "Mr. Watson come here, I want you" > /tmp/safe/bell.txt
```

Just be aware that all writes are being stored in memory (including history) and transferred (kinda slowly) over the network, so don't get too carried away.

An interesting test is to modify a file at the same time on both ends in different ways and verify that the conflict resolves in the same way on both sides.

When finished, use `fusermount -u /tmp/safe` to unmount the filesystem and CTRL-C or kill to end the process.

## License

This Safe Network software is dual-licensed under the Modified BSD (<LICENSE-BSD> <https://opensource.org/licenses/BSD-3-Clause>) or the MIT license (<LICENSE-MIT> <https://opensource.org/licenses/MIT>) at your option.

## Contributing

Want to contribute? Great :tada:

There are many ways to give back to the project, whether it be writing new code, fixing bugs, or just reporting errors. All forms of contributions are encouraged!

For instructions on how to contribute, see our [Guide to contributing](https://github.com/maidsafe/QA/blob/master/CONTRIBUTING.md).
