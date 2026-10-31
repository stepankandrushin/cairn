"""A minimal TLS-terminating TCP proxy, standing in for a reverse proxy in the smoke test."""
import socket
import ssl
import sys
import threading


def pump(src, dst):
    try:
        while data := src.recv(65536):
            dst.sendall(data)
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def main(cert, key, backend):
    host, port = backend.rsplit(":", 1)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(16)
    print(listener.getsockname()[1], flush=True)
    while True:
        client, _ = listener.accept()
        try:
            tls = context.wrap_socket(client, server_side=True)
        except OSError:
            client.close()
            continue
        upstream = socket.create_connection((host, int(port)))
        threading.Thread(target=pump, args=(tls, upstream), daemon=True).start()
        threading.Thread(target=pump, args=(upstream, tls), daemon=True).start()


if __name__ == "__main__":
    main(*sys.argv[1:])
