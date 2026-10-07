# UDP relay that impairs traffic like a home Wi-Fi link.
# viewer -> 127.0.0.1:7800 -> host 127.0.0.1:7700, and back.
import asyncio, random, sys, time
base_ms, jitter_ms, loss, stall_every_s, stall_ms = map(float, sys.argv[1:6])
reorder = float(sys.argv[6]) if len(sys.argv) > 6 else 0.0
rate_mbps = float(sys.argv[7]) if len(sys.argv) > 7 else 0.0  # 0 = unlimited; host->viewer only
MAX_QUEUE_S = 0.08  # drop-tail once 80 ms is queued, like a router
HOST = ("127.0.0.1", 7700)
random.seed(1)
class Relay:
    def __init__(s): s.client=None; s.next_free={0:0.0,1:0.0}; s.stall_until=0.0; s.next_stall=time.monotonic()+stall_every_s if stall_every_s>0 else 1e18  # loop.time() is monotonic too
    def send_later(s, direction, data, addr, sock):
        loop=asyncio.get_running_loop(); now=loop.time()
        if now>=s.next_stall: s.stall_until=now+stall_ms/1000; s.next_stall=now+stall_every_s*random.uniform(0.5,1.5)
        if random.random()<loss: return
        d=(base_ms+random.uniform(-jitter_ms,jitter_ms))/1000
        t=max(now+d, s.next_free[direction], s.stall_until)  # Wi-Fi keeps order; stalls hold the queue
        if rate_mbps and direction==1:
            tx=len(data)*8/(rate_mbps*1e6)
            start=max(now, s.next_free[direction]-d if s.next_free[direction]>now+d else now)
            if s.next_free[direction]-now-d > MAX_QUEUE_S: return  # queue full: tail drop
            t=max(t, s.next_free[direction]+tx)
        t=max(t, s.next_free[direction]+1e-6)
        s.next_free[direction]=t
        if reorder and random.random()<reorder: t+=0.0015  # lands behind the next packet or two
        loop.call_at(t, sock.sendto, data, addr)
r=Relay()
class Front(asyncio.DatagramProtocol):
    def connection_made(s,t): s.t=t
    def datagram_received(s,data,addr):
        r.client=addr; r.send_later(0,data,HOST,back.t)
class Back(asyncio.DatagramProtocol):
    def connection_made(s,t): s.t=t
    def datagram_received(s,data,addr):
        if r.client: r.send_later(1,data,r.client,front.t)
async def main():
    global front, back
    loop=asyncio.get_running_loop()
    _,front=await loop.create_datagram_endpoint(Front, local_addr=("127.0.0.1",7800))
    _,back=await loop.create_datagram_endpoint(Back, local_addr=("127.0.0.1",0))
    await asyncio.sleep(3600)
asyncio.run(main())
