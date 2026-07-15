/**
 * Notifications store — server + app messages for the bell dropdown.
 *
 * ============================================================================
 *  HOW TO SHOW A NOTIFICATION HERE
 * ============================================================================
 *
 *  1. From TypeScript (app side):
 *
 *     import { pushNotification } from "../lib/stores/notifications";
 *     pushNotification({
 *       title: "Backup failed",
 *       body: "Save **factorio** exceeded the 50 MB per-save cap.",
 *       priority: "high",
 *     });
 *
 *  2. From the server (Rust → Tauri event):
 *
 *     The Rust side emits a `hoard://notification` Tauri event with a JSON
 *     payload matching `ServerNotification` (see below). The agent polls the
 *     server's notification endpoint and forwards each new message as an
 *     event; this store listens and pushes it automatically.
 *
 *     To send a notification to users from the server, add a row to the
 *     server's notification table (or broadcast endpoint) with:
 *       - title: string
 *       - body: string  (supports **bold**, *italic*, `code`, [links](url))
 *       - priority: "high" | "normal" | "low"
 *       - audience: "all" | specific user_id (filtering is server-side)
 *
 *     The Rust agent polls every N seconds; new entries are deduped by `id`
 *     so a user never sees the same message twice.
 *
 *  3. Only IMPORTANT notifications should land here. This is NOT the activity
 *     feed (that's `stores/live.ts` → `activityFeed`, for per-save events like
 *     "upload started" / "watcher armed"). This store is for account-level
 *     messages the user needs to act on or know about:
 *       - Server broadcasts (maintenance, new features, security advisories)
 *       - Plan changes (quota reached, trial expiring, upgrade required)
 *       - Critical errors that need user intervention
 *
 *     Routine agent activity (backups, syncs, watcher events) goes in the
 *     activity feed, NOT here. When in doubt: if it can wait, it's a feed
 *     entry; if the user should act on it, it's a notification.
 *
 * ============================================================================
 *  MARKDOWN SUPPORT
 * ============================================================================
 *
 *  The `body` field supports a small, safe subset of markdown:
 *    **bold**          → <strong>
 *    *italic*          → <em>
 *    `inline code`     → <code>
 *    [text](url)       → <a href="url" target="_blank">
 *    \n (line breaks)  → <br>
 *
 *  No raw HTML is allowed — everything is escaped before formatting, so
 *  server messages can't inject scripts. Keep messages short; the panel is
 *  288px wide.
 *
 * ============================================================================
 *  STORAGE
 * ============================================================================
 *
 *  Notifications persist to localStorage (capped at 20, oldest dropped) so
 *  they survive restarts. High-priority ones can't be auto-dismissed; the
 *  user must clear them. Normal/low auto-expire after 7 days.
 */
import { writable } from "svelte/store";

export type NotificationPriority = "high" | "normal" | "low";

export type AppNotification = {
  /** Stable id for dedup. Server notifications use the server's id; app-side
   *  ones use a monotonic counter. */
  id: string;
  title: string;
  body: string;
  priority: NotificationPriority;
  /** Epoch ms. */
  at: number;
  /** "server" = came from the server; "app" = pushed locally. */
  source: "server" | "app";
  /** Optional URL for a CTA button ("Open dashboard", "Upgrade", etc.). */
  action_url?: string;
  action_label?: string;
};

/** Shape of the `hoard://notification` Tauri event payload (from Rust). */
export type ServerNotification = {
  id: string;
  title: string;
  body: string;
  priority?: NotificationPriority;
  action_url?: string;
  action_label?: string;
};

const STORAGE_KEY = "hoard-notifications";
const MAX_ENTRIES = 20;
const EXPIRY_MS = 7 * 86_400_000; // 7 days for normal/low

export const notifications = writable<AppNotification[]>(readStored());

function readStored(): AppNotification[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const all = JSON.parse(raw) as AppNotification[];
    // Drop expired normal/low; keep all high.
    const cutoff = Date.now() - EXPIRY_MS;
    const kept = all.filter(
      (n) => n.priority === "high" || n.at >= cutoff,
    );
    if (kept.length !== all.length) persist(kept);
    return kept;
  } catch {
    return [];
  }
}

function persist(list: AppNotification[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list.slice(0, MAX_ENTRIES)));
  } catch {
    /* storage disabled / quota — the in-memory list still works */
  }
}

let counter = 0;

/** Push a notification from the app side. Deduped by id if provided. */
export function pushNotification(
  n: Omit<AppNotification, "id" | "at" | "source"> & { id?: string },
): void {
  const id = n.id ?? `app-${Date.now()}-${counter++}`;
  notifications.update((list) => {
    if (list.some((x) => x.id === id)) return list;
    const entry: AppNotification = {
      id,
      title: n.title,
      body: n.body,
      priority: n.priority,
      at: Date.now(),
      source: "app",
      action_url: n.action_url,
      action_label: n.action_label,
    };
    const next = [entry, ...list].slice(0, MAX_ENTRIES);
    persist(next);
    return next;
  });
}

/** Dismiss a single notification by id. */
export function dismissNotification(id: string): void {
  notifications.update((list) => {
    const next = list.filter((n) => n.id !== id);
    persist(next);
    return next;
  });
}

/** Clear all. */
export function clearNotifications(): void {
  notifications.set([]);
  persist([]);
}

/** Unread count — for a badge on the bell. Simplified: all non-dismissed are
 *  "unread". A read/unread flag can be added later if needed. */
export { notifications as unreadCount };

let serverListener: (() => void) | null = null;

/** Subscribe to `hoard://notification` Tauri events (server → client). Call
 *  once at boot; the listener lives for the app's lifetime. Idempotent. */
export async function initServerNotifications(): Promise<void> {
  if (serverListener) return;
  try {
    const { listen } = await import("@tauri-apps/api/event");
    const unlisten = await listen<ServerNotification>(
      "hoard://notification",
      (e) => {
        const p = e.payload;
        const id = p.id;
        notifications.update((list) => {
          if (list.some((x) => x.id === id)) return list;
          const entry: AppNotification = {
            id,
            title: p.title,
            body: p.body,
            priority: p.priority ?? "normal",
            at: Date.now(),
            source: "server",
            action_url: p.action_url,
            action_label: p.action_label,
          };
          const next = [entry, ...list].slice(0, MAX_ENTRIES);
          persist(next);
          return next;
        });
      },
    );
    serverListener = unlisten;
  } catch {
    /* Tauri not available (e.g. dev in browser) — no-op */
  }
}

/** Escape HTML, then apply a tiny markdown subset. Returns HTML string safe
 *  to render with {@html}. No raw HTML in the input survives the escape. */
export function renderMarkdown(md: string): string {
  // 1. Escape everything.
  let s = md
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  // 2. Inline code first (so its contents aren't mangled by bold/italic).
  s = s.replace(/`([^`]+)`/g, "<code>$1</code>");
  // 3. Bold.
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  // 4. Italic.
  s = s.replace(/\*([^*]+)\*/g, "<em>$1</em>");
  // 5. Links [text](url) — only http/https, no javascript:.
  s = s.replace(
    /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
    '<a href="$2" target="_blank" rel="noopener noreferrer" class="text-emerald-400 hover:text-emerald-300 underline">$1</a>',
  );
  // 6. Line breaks.
  s = s.replace(/\n/g, "<br>");
  return s;
}
