# Throw garbage at a UDP port: random bytes, and well-formed headers of
# every packet kind with random bodies. Usage: flood.py host port seconds rate
import random, socket, struct, sys, time
host, port, secs, rate = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), int(sys.argv[4])
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
random.seed(9)
end = time.time() + secs
sent = 0
while time.time() < end:
    t0 = time.time()
    for _ in range(rate // 100):
        r = random.random()
        if r < 0.4:
            d = bytes(random.getrandbits(8) for _ in range(random.randrange(0, 1400)))
        else:
            kind = random.randrange(0, 14)
            n = random.randrange(1, 65535) if random.random() < 0.1 else random.randrange(1, 40)
            ix = random.randrange(0, n)
            body = bytes(random.getrandbits(8) for _ in range(random.randrange(0, 1188)))
            if kind == 4 and random.random() < 0.5:   # control: plausible JSON, never a Hello
                body = random.choice([b'{"type":"discover"}', b'{"type":"bye"}', b'{"type":"set_max_bitrate","kbps":1}',
                                      b'{"type":"set_host_mute","muted":true}', b'{"type":"here","name":"x"}', b'{'])
            d = struct.pack(">BBHIHH", kind, random.randrange(256), random.randrange(65536),
                            random.randrange(2**32), ix, n) + body
        try:
            s.sendto(d, (host, port)); sent += 1
        except OSError:
            pass
    time.sleep(max(0, 0.01 - (time.time() - t0)))
print("sent", sent)
