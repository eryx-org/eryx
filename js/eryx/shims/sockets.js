/**
 * Sockets shims for the eryx sandbox.
 *
 * The Python sandbox imports wasi:sockets/* but doesn't actually use them
 * (networking goes through eryx:net/tcp and eryx:net/tls instead).
 * These stubs satisfy the imports without providing real functionality.
 */

class ResolveAddressStream {}
class TcpSocket {}
class UdpSocket {}
class IncomingDatagramStream {}
class OutgoingDatagramStream {}
class Network {}

export const instanceNetwork = {
  instanceNetwork() {
    return new Network();
  },
};

export const ipNameLookup = {
  ResolveAddressStream,
  resolveAddresses() {
    throw new Error("networking not available in sandbox");
  },
};

export const network = {
  Network,
};

export const tcpCreateSocket = {
  createTcpSocket() {
    throw new Error("networking not available in sandbox");
  },
};

export const tcp = {
  TcpSocket,
};

export const udpCreateSocket = {
  createUdpSocket() {
    throw new Error("networking not available in sandbox");
  },
};

export const udp = {
  UdpSocket,
  IncomingDatagramStream,
  OutgoingDatagramStream,
};
