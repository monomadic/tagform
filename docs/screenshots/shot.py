import os, pty, sys, time, select, struct, fcntl, termios, signal, json, html
import pyte

COLS, ROWS = int(os.environ.get("COLS", 100)), int(os.environ.get("ROWS", 30))

def run(argv, steps, out):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLORTERM"] = "truecolor"
        os.environ.pop("TERM_PROGRAM", None)
        os.execvp(argv[0], argv)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    def pump(t):
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data: return
                stream.feed(data)
                if b"\x1b[c" in data or b"\x1b[0c" in data:
                    os.write(fd, b"\x1b[?62;22c")
                if b"\x1b[6n" in data:
                    os.write(fd, f"\x1b[{screen.cursor.y+1};{screen.cursor.x+1}R".encode())
                if b"\x1b[16t" in data:
                    os.write(fd, b"\x1b[6;18;8t")
                if b"\x1b[14t" in data:
                    os.write(fd, f"\x1b[4;{ROWS*18};{COLS*8}t".encode())
    pump(1.5)
    for keys in steps:
        os.write(fd, keys.encode() if isinstance(keys, str) else keys)
        pump(0.6)
    snap = []
    for y in range(ROWS):
        row = []
        for x in range(COLS):
            c = screen.buffer[y][x]
            row.append((c.data, c.fg, c.bg, c.bold, c.italics, c.underscore, c.reverse))
        snap.append(row)
    json.dump(snap, open(out, "w"))
    os.write(fd, b"\x03"); pump(0.3)
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass

if __name__ == "__main__":
    out = sys.argv[1]; steps = json.loads(sys.argv[2], strict=False); argv = sys.argv[3:]
    run(argv, steps, out)
