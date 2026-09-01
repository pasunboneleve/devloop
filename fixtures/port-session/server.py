import http.server
import sys


host = "127.0.0.1"
port = int(sys.argv[1])
server = http.server.ThreadingHTTPServer((host, port), http.server.SimpleHTTPRequestHandler)
print(f"listening {server.server_address[1]}", flush=True)
server.serve_forever()
