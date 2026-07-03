/**
 * Gateway WebSocket client.
 *
 * One socket, exponential backoff reconnect, typed frame dispatch. The
 * client never interprets payloads — it hands parsed frames to the store.
 */

/** Initial reconnect delay. */
const RECONNECT_BASE_MS = 250
/** Reconnect delay ceiling. */
const RECONNECT_MAX_MS = 8_000

export interface GatewayFrame {
  type: string
  [key: string]: unknown
}

export type FrameHandler = (frame: GatewayFrame) => void
export type StatusHandler = (connected: boolean) => void

export class GatewayClient {
  private ws: WebSocket | null = null
  private backoffMs = RECONNECT_BASE_MS
  private closed = false
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null

  constructor(
    private readonly url: string,
    private readonly onFrame: FrameHandler,
    private readonly onStatus: StatusHandler,
  ) {}

  connect(): void {
    if (this.closed) return
    const ws = new WebSocket(this.url)
    this.ws = ws

    ws.onopen = () => {
      this.backoffMs = RECONNECT_BASE_MS
      this.onStatus(true)
    }

    ws.onmessage = (ev: MessageEvent<string>) => {
      let frame: GatewayFrame
      try {
        frame = JSON.parse(ev.data) as GatewayFrame
      } catch {
        // A malformed frame is a gateway bug; drop it loudly.
        console.error('gateway sent unparseable frame', ev.data.slice(0, 200))
        return
      }
      this.onFrame(frame)
    }

    ws.onclose = () => {
      this.onStatus(false)
      this.scheduleReconnect()
    }

    ws.onerror = () => {
      ws.close()
    }
  }

  private scheduleReconnect(): void {
    if (this.closed || this.reconnectTimer !== null) return
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, this.backoffMs)
    this.backoffMs = Math.min(this.backoffMs * 2, RECONNECT_MAX_MS)
  }

  close(): void {
    this.closed = true
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer)
    this.ws?.close()
  }
}
