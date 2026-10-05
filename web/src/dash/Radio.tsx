// The radio system as heard: which talkgroups are busy (and which are new,
// unknown, or ignored), which radios are on the air, what they're
// affiliated with and who they talk with — and, for one talkgroup or one
// radio, its week and its connections.

import { createContext, Fragment, useContext, useMemo, useState } from "react";
import { systemColor } from "../config.ts";
import { radioQuery, setTalkgroupIgnore, setView, updateConfig, usePolled, useApp, type AppState } from "../controller.ts";
import { Bipartite, Card, Choice, Heatmap, HeatStrip, Histogram, Light, RadialEgo, Stat, type Node } from "../charts.tsx";
import { ago, clockAt, compact, dur, hmm, num } from "../fmt.ts";
import type { Config, EncState, LengthHistogram, LinkRow, RadioResult, TgRow, UnitEventRow, UnitRow } from "../protocol.ts";
import { aliasOf } from "./Calls.tsx";
import { dashSystems, K, KIND_LABEL, running, total, useHistory, type DashSystem } from "./data.ts";

type Hours = "1" | "24" | "168" | "720" | "2160" | "8760" | "0";
const HOURS: { v: Hours; label: string }[] = [
  { v: "1", label: "1 h" },
  { v: "24", label: "24 h" },
  { v: "168", label: "7 days" },
  { v: "720", label: "30 days" },
  { v: "2160", label: "90 days" },
  { v: "8760", label: "1 year" },
  { v: "0", label: "All" },
];
const spanText = (h: Hours) => (h === "0" ? "all time" : h === "1" ? "last hour" : `last ${HOURS.find((x) => x.v === h)?.label ?? `${h} h`}`);

function stored(key: string, fallback: string): string {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}
function store(key: string, v: string) {
  try {
    localStorage.setItem(key, v);
  } catch {
    /* private window: not remembered */
  }
}

/** The span the radio pages show (each browser keeps its own). */
function useSpan(): [Hours, (h: Hours) => void] {
  const [h, setH] = useState<Hours>(() => {
    const v = stored("trp.radio.span", "24") as Hours;
    return HOURS.some((x) => x.v === v) ? v : "24";
  });
  return [
    h,
    (v) => {
      store("trp.radio.span", v);
      setH(v);
    },
  ];
}

/** A system's name: its name in the config, else what its control channels say ("BEE00:3A9"). */
export function systemLabel(key: string, config: Config | null | undefined): string {
  const named = config?.systemNames?.[key];
  if (named) return named;
  const p25 = /^p25:([0-9a-f]+)\.([0-9a-f]+)$/i.exec(key);
  if (p25) return `${p25[1].toUpperCase()}:${p25[2].toUpperCase()}`;
  const sn = /^smartnet:([0-9a-f]+)$/i.exec(key);
  if (sn) return `SmartNet ${sn[1].toUpperCase()}`;
  return key.replace(/^(group|cfg):/, "");
}

/** What the key says, in words ("WACN BEE00 · SysID 3A9"). */
function keyText(key: string): string {
  const p25 = /^p25:([0-9a-f]+)\.([0-9a-f]+)$/i.exec(key);
  if (p25) return `WACN ${p25[1].toUpperCase()} · SysID ${p25[2].toUpperCase()}`;
  const sn = /^smartnet:([0-9a-f]+)$/i.exec(key);
  if (sn) return `SysID ${sn[1].toUpperCase()}`;
  return key.startsWith("group:") ? `site group ${key.slice(6)}` : key;
}

/** What a page shows: one site, or (at its key) every site of a system. `query` is also where its links go. */
interface Scope {
  /** radioQuery's `system`, and the page's place: a site's short name, or a system's key. */
  query: string;
  /** Every site of the system. */
  all: boolean;
  /** The system has several sites. */
  multi: boolean;
  key?: string;
  sites: string[];
  label: string;
}

const ScopeCtx = createContext<Scope>({ query: "", all: false, multi: false, sites: [], label: "" });
/** Where the page's links go: the site or system it shows. */
const useNav = () => useContext(ScopeCtx).query;

/** Network colours: encrypted (any, in the span) red; off or not heard in a day gray; else the chart's own. */
const NET_ENC = "var(--net-enc)";
const NET_STALE = "var(--net-stale)";
const encrypted = (e: EncState | undefined) => !!e && e !== "clear";
const stale = (last: number, state?: "on" | "off" | null) => state === "off" || Date.now() / 1000 - last > 86400;

const WHY = {
  tg: "A talkgroup with only clear calls in this span",
  radio: "A radio that's on, heard in the last day, with only clear calls in this span",
  enc: "Encrypted: at least one encrypted call in this span (a link: on that talkgroup)",
  stale: "Off (the radio deregistered and hasn't been heard since), or not heard for more than a day",
};

function NetLegend() {
  return (
    <div className="net-legend small muted">
      <span title={WHY.tg}>
        <i style={{ background: "var(--series-1)" }} />
        Talkgroup, clear
      </span>
      <span title={WHY.radio}>
        <i style={{ background: "var(--series-3)" }} />
        Radio, clear
      </span>
      <span title={WHY.enc}>
        <i style={{ background: NET_ENC }} />
        Encrypted (any, this span)
      </span>
      <span title={WHY.stale}>
        <i style={{ background: NET_STALE }} />
        Off, or not heard in a day
      </span>
      <span>Hover a dot or line for why.</span>
    </div>
  );
}

/** Why a node has its colour, for its hover. */
function netWhy(stale: boolean, offNow: boolean, enc: EncState | undefined, encN?: number, n?: number): string {
  if (offNow) return "off: deregistered" + (encrypted(enc) ? " (had encrypted calls)" : "");
  if (stale) return "not heard for more than a day";
  if (enc === "encrypted") return "encrypted: all or nearly all calls";
  if (enc === "mixed") return `encrypted sometimes${encN !== undefined && n ? `: ${encN} of ${n} calls` : ""}`;
  return "clear";
}

function EncBadge({ e, pct }: { e?: EncState; pct?: number }) {
  const state = e ?? (pct !== undefined && pct > 50 ? "encrypted" : pct ? "mixed" : "clear");
  if (state === "clear") return null;
  return state === "encrypted" ? (
    <span className="badge enc" title="Encrypted: all or nearly all (95% or more) of its calls">
      ENC
    </span>
  ) : (
    <span className="badge enc mixed" title={`Encrypted sometimes: some of its calls were${pct ? ` (${pct}%)` : ""}, the rest clear`}>
      ENC SOME
    </span>
  );
}

function StateBadge({ state, last }: { state?: "on" | "off" | null; last: number }) {
  if (state === "off")
    return (
      <span className="badge state-off" title="Off: it deregistered, and hasn't been heard since">
        OFF
      </span>
    );
  if (Date.now() / 1000 - last > 86400)
    return (
      <span className="badge state-off" title="Idle: not heard for more than a day">
        IDLE
      </span>
    );
  if (state === "on")
    return (
      <span className="badge state-on" title="On: registered, or heard doing something since it last deregistered">
        ON
      </span>
    );
  return null;
}

/** Why calls weren't recorded (and were), in the order shown. */
const REASONS: { k: string; label: string; color: string; hint: string }[] = [
  { k: "recorded", label: "Recorded", color: "var(--series-1)", hint: "" },
  { k: "monitored", label: "Followed only", color: "var(--series-3)", hint: "Followed, not recorded" },
  { k: "ignored", label: "Ignored", color: "var(--muted-mark)", hint: "Set to Ignore in the talkgroup file" },
  { k: "encrypted", label: "Encrypted", color: "var(--series-7)", hint: "Encrypted calls are off" },
  { k: "unknown_tg", label: "Not in the file", color: "var(--series-4)", hint: "Unknown talkgroups are off" },
  { k: "no_recorder", label: "No recorder", color: "var(--critical)", hint: "All recorders busy" },
  { k: "no_source", label: "Out of band", color: "var(--serious)", hint: "No source covers the frequency" },
];

/** Point `i` of `n`, `per` hours each, ending this hour: when it began. */
const bucketLabel = (n: number, per = 1) => (i: number) => {
  const t = (Math.floor(Date.now() / 3600_000) - (n - i) * per + 1) * 3600;
  const day = new Date(t * 1000).toLocaleDateString([], { month: "short", day: "numeric" });
  return per >= 24 ? day : per > 1 ? `${day} ${clockAt(t)}` : clockAt(t);
};

function TgName({ r }: { r: { tg: number; alphaTag: string } }) {
  return r.alphaTag ? (
    <span className="tg-name" title={`Talkgroup ${r.tg}`}>
      {r.alphaTag}
    </span>
  ) : (
    <span className="tg">{r.tg}</span>
  );
}

function IgnoreToggle({ sys, r }: { sys: DashSystem; r: TgRow }) {
  const sc = useContext(ScopeCtx);
  if (sys.kind === "conventional") return null;
  return (
    <button
      className={`btn ghost small${r.ignore ? " on" : ""}`}
      onClick={(e) => {
        e.stopPropagation();
        for (const name of sc.all ? sc.sites : [sys.name]) setTalkgroupIgnore(name, r.tg, !r.ignore, r.alphaTag);
      }}
      title={r.ignore ? "Record again" : "Stop recording"}
    >
      {r.ignore ? "Ignored ✕" : "Ignore"}
    </button>
  );
}

// ── the system ───────────────────────────────────────────────────────────────

function Reasons({ name, scope, hours }: { name: string; scope: string; hours: Hours }) {
  const db = usePolled(`reasons:${scope}:${hours}`, () => radioQuery({ what: "reasons", system: scope, hours: Number(hours) }), 60_000);
  const hist = useHistory(REASONS.map((r) => K.sys(name, `why/${r.k}`)), hours === "1" || hours === "24" ? "24h" : "7d", 48);
  const counts = REASONS.map((r) => (db?.counts ? (db.counts[r.k] ?? 0) : Math.round(total(hist?.[K.sys(name, `why/${r.k}`)]) ?? 0)));
  const sum = counts.reduce((a, b) => a + b, 0);
  return (
    <Card title="What happened to calls">
      {sum === 0 ? (
        <p className="empty">No calls yet.</p>
      ) : (
        <>
          <div className="stackbar" role="img" aria-label="calls by what happened">
            {REASONS.map((r, i) =>
              counts[i] > 0 ? <span key={r.k} style={{ flexGrow: counts[i], background: r.color }} title={`${r.label}: ${Math.round(counts[i])} (${Math.round((100 * counts[i]) / sum)}%)`} /> : null,
            )}
          </div>
          <ul className="legend-list">
            {REASONS.map((r, i) =>
              counts[i] > 0 ? (
                <li key={r.k} title={r.hint}>
                  <i style={{ background: r.color }} />
                  {r.label}
                  <b>{compact(counts[i])}</b>
                  <span className="muted">{Math.round((100 * counts[i]) / sum)}%</span>
                </li>
              ) : null,
            )}
          </ul>
        </>
      )}
    </Card>
  );
}

function ActiveNow({ s, sys, sites }: { s: AppState; sys: DashSystem; sites: string[] }) {
  const nav = useNav();
  const calls = s.calls.filter((c) => sites.includes(c.systemName));
  return (
    <Card title="On the air now">
      {calls.length === 0 ? (
        <p className="empty">Quiet.</p>
      ) : (
        <ul className="air">
          {calls.map((c) => (
            <li key={c.id}>
              <button className="linkish" onClick={() => setView("radio", nav, "tg", c.talkgroup)}>
                <span className={`dot dot-${c.state}${c.encrypted ? " dot-enc" : ""}`} />
                <TgName r={{ tg: c.talkgroup, alphaTag: c.alphaTag }} />
                {sites.length > 1 && <span className="muted small"> {c.systemName}</span>}
              </button>
              <span className="row">
                {[...new Set(c.sources)].slice(-4).map((u) => (
                  <button key={u} className="chip" onClick={() => setView("radio", nav, "unit", u)}>
                    {aliasOf(s, sys.name, u) ?? u}
                  </button>
                ))}
              </span>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

type TgFilter = "all" | "new" | "unknown" | "ignored" | "encrypted";

const hasEnc = (r: TgRow) => (r.enc ? r.enc !== "clear" : r.encPct > 0);

function Talkgroups({ sys, rows, label }: { sys: DashSystem; rows: TgRow[]; label: (i: number) => string }) {
  const nav = useNav();
  const [filter, setFilter] = useState<TgFilter>("all");
  const [q, setQ] = useState("");
  const [sort, setSort] = useState<"secs" | "calls" | "last" | "first">("secs");
  const shown = useMemo(() => {
    const f = q.trim().toLowerCase();
    return rows
      .filter((r) => (filter === "all" ? true : filter === "new" ? r.new : filter === "unknown" ? !r.known : filter === "ignored" ? r.ignore : hasEnc(r)))
      .filter((r) => !f || String(r.tg).includes(f) || r.alphaTag.toLowerCase().includes(f))
      .sort((a, b) => (sort === "first" ? b.first - a.first : sort === "last" ? b.last - a.last : b[sort] - a[sort]));
  }, [rows, filter, q, sort]);
  const n = (k: TgFilter) => rows.filter((r) => (k === "new" ? r.new : k === "unknown" ? !r.known : k === "ignored" ? r.ignore : hasEnc(r))).length;
  return (
    <Card
      title="Talkgroups"
      actions={
        <>
          <input className="search" placeholder="Find a talkgroup…" value={q} onChange={(e) => setQ(e.target.value)} />
        </>
      }
    >
      <div className="row">
        <Choice
          value={filter}
          onChange={setFilter}
          label="Show"
          options={[
            { v: "all", label: `All ${rows.length}` },
            { v: "new", label: `New ${n("new")}` },
            { v: "unknown", label: `Unknown ${n("unknown")}` },
            { v: "ignored", label: `Ignored ${n("ignored")}` },
            { v: "encrypted", label: `Encrypted ${n("encrypted")}` },
          ]}
        />
        <span className="spacer" />
        <Choice
          value={sort}
          onChange={setSort}
          label="Sort"
          options={[
            { v: "secs", label: "Airtime" },
            { v: "calls", label: "Calls" },
            { v: "last", label: "Latest" },
            { v: "first", label: "Newest" },
          ]}
        />
      </div>
      <div className="table-wrap tall">
        <table className="calls clickable-rows">
          <thead>
            <tr>
              <th>Talkgroup</th>
              <th>Calls</th>
              <th>Airtime</th>
              <th>Activity</th>
              <th>Last heard</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {shown.slice(0, 300).map((r) => (
              <tr key={r.tg} className={r.ignore ? "ignored" : ""} onClick={() => setView("radio", nav, "tg", r.tg)}>
                <td>
                  <TgName r={r} />
                  {r.alphaTag && <span className="tag mono">{r.tg}</span>}
                  {r.new && <span className="badge new">NEW</span>}
                  {!r.known && <span className="badge unknown">NOT IN FILE</span>}
                  <EncBadge e={r.enc} pct={r.encPct} />
                  {r.ignore && <span className="badge ign">IGNORED</span>}
                </td>
                <td className="mono">{r.calls}</td>
                <td className="mono">{dur(r.secs)}</td>
                <td className="spark-cell">
                  <HeatStrip v={r.hourly} titles={r.hourly.map((v, i) => `${label(i)}: ${v} calls`)} />
                </td>
                <td className="small">{ago(r.last)}</td>
                <td>
                  <IgnoreToggle sys={sys} r={r} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {shown.length > 300 && <p className="muted small">300 of {shown.length} shown.</p>}
    </Card>
  );
}

function Units({ s, sys, rows, total: n, sites }: { s: AppState; sys: DashSystem; rows: UnitRow[]; total: number; sites: boolean }) {
  const nav = useNav();
  const [q, setQ] = useState("");
  const f = q.trim().toLowerCase();
  const shown = rows.filter((r) => !f || String(r.unit).includes(f) || (aliasOf(s, sys.name, r.unit) ?? r.alias).toLowerCase().includes(f));
  return (
    <Card title={`Radios (${compact(n)})`} actions={<input className="search" placeholder="Find a radio…" value={q} onChange={(e) => setQ(e.target.value)} />}>
      <div className="table-wrap tall">
        <table className="calls clickable-rows">
          <thead>
            <tr>
              <th>Radio</th>
              <th>State</th>
              {sites && <th>Site</th>}
              <th>Affiliated</th>
              <th>Transmitted</th>
              <th>Talks on</th>
              <th>Last heard</th>
            </tr>
          </thead>
          <tbody>
            {shown.slice(0, 300).map((r) => {
              const alias = aliasOf(s, sys.name, r.unit) ?? r.alias;
              return (
                <tr key={r.unit} onClick={() => setView("radio", nav, "unit", r.unit)}>
                  <td>
                    {alias ? <b>{alias}</b> : null} <span className="mono muted">{r.unit}</span>
                    {r.new && <span className="badge new">NEW</span>}
                    <EncBadge e={r.spanEnc ?? r.enc} />
                  </td>
                  <td>
                    <StateBadge state={r.state} last={r.last} />
                  </td>
                  {sites && <td className="small">{r.lastSite ?? "—"}</td>}
                  <td className="mono">{r.aff ?? "—"}</td>
                  <td className="mono">
                    {r.tx} · {dur(r.txSecs)}
                  </td>
                  <td className="small">{r.tgs.slice(0, 3).map(([tg, k]) => `${tg}×${k}`).join(" · ") || "—"}</td>
                  <td className="small">{ago(r.lastTx ?? r.last)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

/** Who talks on what: the busiest talkgroups and the radios on them. */
function Network({ s, sys, tgs, units }: { s: AppState; sys: DashSystem; tgs: TgRow[]; units: UnitRow[] }) {
  const nav = useNav();
  const top = units.filter((u) => u.tgs.length).sort((a, b) => b.tx - a.tx).slice(0, 18);
  const tgSet = new Map<number, number>();
  for (const u of top) for (const [tg, n] of u.tgs) tgSet.set(tg, (tgSet.get(tg) ?? 0) + n);
  const tgTop = [...tgSet].sort((a, b) => b[1] - a[1]).slice(0, 14);
  const row = (tg: number) => tgs.find((r) => r.tg === tg);
  const name = (tg: number) => row(tg)?.alphaTag || String(tg);
  const tgColor = (tg: number) => {
    const r = row(tg);
    return !r ? undefined : stale(r.last) ? NET_STALE : hasEnc(r) ? NET_ENC : undefined;
  };
  const tgWhy = (tg: number) => {
    const r = row(tg);
    if (!r) return undefined;
    return `TG ${tg} · ${r.calls} calls · ${netWhy(stale(r.last), false, r.enc ?? (hasEnc(r) ? "mixed" : "clear"), r.encCalls, r.calls)}`;
  };
  const left: Node[] = tgTop.map(([tg, n]) => ({ id: `t${tg}`, label: name(tg), weight: n, color: tgColor(tg), sub: tgWhy(tg) }));
  const right: Node[] = top.map((u) => ({
    id: `u${u.unit}`,
    label: aliasOf(s, sys.name, u.unit) || u.alias || String(u.unit),
    weight: u.tx,
    color: stale(u.last, u.state) ? NET_STALE : encrypted(u.spanEnc ?? u.enc) ? NET_ENC : undefined,
    sub: `unit ${u.unit} · ${u.spanTx ?? u.tx} transmissions · ${netWhy(stale(u.last, u.state), u.state === "off", u.spanEnc ?? u.enc)}`,
  }));
  const edges = top.flatMap((u) =>
    u.tgs
      .filter(([tg]) => tgSet.has(tg) && tgTop.some((t) => t[0] === tg))
      .map(([tg, n, enc]) => ({
        a: `t${tg}`,
        b: `u${u.unit}`,
        w: n,
        color: enc ? NET_ENC : undefined,
        title: `${name(tg)} ↔ ${aliasOf(s, sys.name, u.unit) || u.alias || u.unit}: met ${n} times (calls, joins, locations)${enc ? ` · ${enc} encrypted calls` : " · clear"}`,
      })),
  );
  if (left.length < 2 || right.length < 2) return null;
  return (
    <Card title="Who talks on what">
      <Bipartite
        left={left}
        right={right}
        edges={edges}
        leftTitle="Talkgroups"
        rightTitle="Radios (busiest)"
        onClick={(n) => (n.id.startsWith("t") ? setView("radio", nav, "tg", n.id.slice(1)) : setView("radio", nav, "unit", n.id.slice(1)))}
      />
      <NetLegend />
    </Card>
  );
}

const EVENT_LABEL: Record<UnitEventRow["kind"], string> = { on: "On", off: "Off", join: "Joined", location: "Location", data: "Data", ack: "Ack" };

/** Radios' on / off, joins and locations as they happen (radio database). */
function RadioEvents({ sys, scope, sites }: { sys: DashSystem; scope: string; sites: boolean }) {
  const nav = useNav();
  const r = usePolled(`events:${scope}`, () => radioQuery({ what: "events", system: scope, hours: 1, limit: 60 }), 15_000);
  const rows = (r?.rows ?? []) as UnitEventRow[];
  if (!r || r.error) return null;
  return (
    <Card title="Radios coming and going">
      {rows.length === 0 ? (
        <p className="empty">Nothing in the last hour.</p>
      ) : (
        <ul className="timeline">
          {rows.map((e, i) => (
            <li key={i}>
              <span className="muted small">{clockAt(e.t, true)}</span>
              <span className={`badge ${e.kind === "off" ? "state-off" : e.kind === "on" ? "state-on" : "ign"}`}>{EVENT_LABEL[e.kind]}</span>
              <button className="linkish" onClick={() => setView("radio", nav, "unit", e.unit!)}>
                {e.alias || e.unit}
              </button>
              {e.tg !== null && (
                <button className="linkish small" onClick={() => setView("radio", nav, "tg", e.tg!)}>
                  → {e.alphaTag || e.tg}
                </button>
              )}
              {sites && e.site && <span className="muted small">{e.site}</span>}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function Lengths({ h }: { h: LengthHistogram | undefined }) {
  if (!h) return null;
  const labels = h.edges.map((e, i) => (i === h.edges.length - 1 ? `${e}+ s` : `${e}–${h.edges[i + 1]}`));
  return (
    <Card title="Call lengths">
      <Histogram counts={h.counts} labels={labels} title="calls by length, seconds" />
    </Card>
  );
}

function SystemView({ sc, s, sys }: { sc: Scope; s: AppState; sys: DashSystem }) {
  const nav = useNav();
  const [hours, setHours] = useSpan();
  const h = Number(hours);
  const q = sc.query;
  const live = running(s);
  const every = live ? 30_000 : 0;
  const tgsR = usePolled(`tgs:${q}:${hours}`, () => radioQuery({ what: "talkgroups", system: q, hours: h, limit: 2000 }), every);
  const unitsR = usePolled(`units:${q}:${hours}`, () => radioQuery({ what: "units", system: q, hours: h, limit: 1000 }), every);
  const sumR = usePolled(`sum:${sys.name}`, () => radioQuery({ what: "summary" }), every * 2);
  const lenR = usePolled(`len:${q}:${hours}`, () => radioQuery({ what: "lengths", system: q, hours: h }), every * 2);
  const tgs = (tgsR?.rows ?? []) as TgRow[];
  const units = (unitsR?.rows ?? []) as UnitRow[];
  const summary = sumR?.systems?.[q];
  const label = bucketLabel(tgs[0]?.hourly.length ?? 24, tgsR?.bucketHours ?? 1);
  const calls = tgs.reduce((a, r) => a + r.calls, 0);
  const secs = tgs.reduce((a, r) => a + r.secs, 0);
  const busy = [...tgs].sort((a, b) => b.calls - a.calls).slice(0, 40);
  const error = tgsR?.error;
  return (
    <div className="stack">
      <div className="row">
        <span className="spacer" />
        <Choice value={hours} options={HOURS} onChange={setHours} label="Span" />
      </div>
      {error && <p className="empty">{error}</p>}
      <div className="kpis">
        <Stat size="hero" label="Calls" value={compact(calls)} sub={sc.all ? `${spanText(hours)} · all sites, each call once` : spanText(hours)} />
        <Stat label="Airtime" value={hmm(secs)} sub={calls ? `${dur(secs / calls)} avg call` : undefined} />
        <Stat label="Talkgroups heard" value={compact(tgsR?.total ?? tgs.length)} sub={summary ? `${summary.talkgroups} known` : undefined} />
        <Stat label="Radios heard" value={compact(unitsR?.total ?? units.length)} sub={summary ? `${summary.units1h} last hour` : undefined} />
        <Stat
          label="New talkgroups"
          value={summary ? summary.newTgs.length : "—"}
          level={summary && summary.newTgs.length ? "warn" : undefined}
          sub={summary?.baseline === false ? "learning (first hour)" : "first heard in 24 h"}
        />
        <Stat label="Unknown" value={summary ? summary.unknownTgs24h : "—"} sub="not in the talkgroup file" />
      </div>
      <div className="columns">
        <Reasons name={sys.name} scope={q} hours={hours} />
        <ActiveNow s={s} sys={sys} sites={sc.all ? sc.sites : [sys.name]} />
      </div>
      {busy.length > 0 && (
        <Card title={`Busiest ${busy.length}${(tgsR?.bucketHours ?? 1) > 1 ? "" : " by hour"}`}>
          <Heatmap
            rows={busy.map((r) => ({ label: r.alphaTag || String(r.tg), sub: `${r.calls} calls`, v: r.hourly }))}
            colLabel={label}
            valueText={(v) => `${v} call${v === 1 ? "" : "s"}`}
            onRow={(i) => setView("radio", nav, "tg", busy[i].tg)}
          />
        </Card>
      )}
      <Network s={s} sys={sys} tgs={tgs} units={units} />
      <Talkgroups sys={sys} rows={tgs} label={label} />
      <div className="columns">
        <Units s={s} sys={sys} rows={units} total={unitsR?.total ?? units.length} sites={sc.all} />
        <div className="stack">
          <Lengths h={lenR?.histogram} />
          <RadioEvents sys={sys} scope={q} sites={sc.all} />
        </div>
      </div>
    </div>
  );
}

// ── one talkgroup ────────────────────────────────────────────────────────────

function TgView({ sc, s, sys, tg }: { sc: Scope; s: AppState; sys: DashSystem; tg: number }) {
  const nav = useNav();
  const [hours, setHours] = useSpan();
  const r = usePolled(`tg:${sc.query}:${tg}:${hours}`, () => radioQuery({ what: "tg", system: sc.query, key: tg, hours: Number(hours) }), running(s) ? 30_000 : 0);
  const row = r?.row as TgRow | null | undefined;
  if (!r) return <p className="empty">Loading…</p>;
  if (!row) return <p className="empty">Talkgroup {tg} hasn't been heard on {sc.label}.</p>;
  // The week's hours laid out by local day (a row each, today at the bottom) and local hour.
  const week = r.week ?? [];
  const nowH = Math.floor(Date.now() / 3600_000);
  const byDay = new Map<string, { label: string; v: number[] }>();
  week.forEach((v, j) => {
    const d = new Date((nowH - (week.length - 1) + j) * 3600_000);
    const k = d.toDateString();
    if (!byDay.has(k)) byDay.set(k, { label: d.toLocaleDateString([], { weekday: "short", day: "numeric" }), v: Array(24).fill(0) });
    byDay.get(k)!.v[d.getHours()] += v;
  });
  const days = [...byDay.values()];
  const talkers = r.talkers ?? [];
  const maxN = Math.max(1, ...talkers.map((t) => t.n));
  return (
    <div className="stack">
      <div className="row">
        <button className="btn ghost small" onClick={() => setView("radio", nav)}>
          ← {sc.label}
        </button>
        <span className="spacer" />
        <Choice value={hours} options={HOURS} onChange={setHours} label="Span" />
      </div>
      <section className="hero">
        <div>
          <h2 className="big">{row.alphaTag || `Talkgroup ${tg}`}</h2>
          <p className="muted">
            TG {tg} · {sc.label} · first heard {clockAt(row.first)} · last {ago(row.last)}
            {!row.known && " · not in the talkgroup file"}
          </p>
        </div>
        <div className="row">
          <EncBadge e={row.totalEnc ?? row.enc} pct={row.encPct} />
          {row.ignore && <span className="badge ign">IGNORED</span>}
          <IgnoreToggle sys={sys} r={row} />
        </div>
      </section>
      <div className="kpis">
        <Stat label="Calls" value={compact(row.calls)} sub={`${spanText(hours)} · ${row.totalCalls} since first heard`} />
        <Stat label="Airtime" value={hmm(row.secs)} sub={spanText(hours)} />
        <Stat label="Encrypted" value={`${row.encPct}%`} sub={row.encCalls !== undefined ? `${row.encCalls} calls` : undefined} />
        <Stat label="Affiliated now" value={(r.affiliated ?? []).length} sub="radios" />
      </div>
      <Card title="Last 7 days">
        <Heatmap rows={days} colLabel={(i) => `${String(i).padStart(2, "0")}:00–${String(i + 1).padStart(2, "0")}:00`} valueText={(v) => `${v} calls`} rowH={22} />
      </Card>
      <div className="columns">
        <Card title="Who talks here">
          {talkers.length === 0 ? (
            <p className="empty">No radio IDs heard on it.</p>
          ) : (
            <ul className="barlist">
              {talkers.map((t) => (
                <li key={t.unit}>
                  <button className="linkish" onClick={() => setView("radio", nav, "unit", t.unit)}>
                    {aliasOf(s, sys.name, t.unit) ?? (t.alias || t.unit)}
                  </button>
                  <span className="bar" style={{ width: `${(t.n / maxN) * 100}%` }} />
                  <b className="mono">{t.n}</b>
                </li>
              ))}
            </ul>
          )}
        </Card>
        <div className="stack">
          <Lengths h={r.lengths} />
          <Links s={s} sys={sys} links={r.links} />
          <Card title="Affiliated with it">
            {(r.affiliated ?? []).length === 0 ? (
              <p className="empty">None right now.</p>
            ) : (
              <div className="row">
                {(r.affiliated ?? []).map((a) => (
                  <button key={a.unit} className="chip" onClick={() => setView("radio", nav, "unit", a.unit)}>
                    {aliasOf(s, sys.name, a.unit) ?? (a.alias || a.unit)}
                  </button>
                ))}
              </div>
            )}
          </Card>
        </div>
      </div>
    </div>
  );
}

// ── one radio ────────────────────────────────────────────────────────────────

function UnitView({ sc, s, sys, unit }: { sc: Scope; s: AppState; sys: DashSystem; unit: number }) {
  const nav = useNav();
  const r = usePolled(`unit:${sc.query}:${unit}`, () => radioQuery({ what: "unit", system: sc.query, key: unit }), running(s) ? 15_000 : 0) as RadioResult | null;
  const row = r?.row as UnitRow | null | undefined;
  if (!r) return <p className="empty">Loading…</p>;
  if (!row) return <p className="empty">Radio {unit} hasn't been heard on {sc.label}.</p>;
  const alias = aliasOf(s, sys.name, unit) ?? row.alias;
  const tgs = r.tgs ?? [];
  const partners = r.partners ?? [];
  const affTag = row.aff !== null ? (tgs.find((t) => t.tg === row.aff)?.alphaTag ?? "") : "";
  return (
    <div className="stack">
      <div className="row">
        <button className="btn ghost small" onClick={() => setView("radio", nav)}>
          ← {sc.label}
        </button>
      </div>
      <section className="hero">
        <div>
          <h2 className="big">{alias || `Radio ${unit}`}</h2>
          <p className="muted">
            Unit {unit} · {sc.label} · first heard {clockAt(row.first)} · last {ago(row.last)}
            {row.lastSite && sc.multi ? ` on ${row.lastSite}` : ""}
            {row.state === undefined && row.reg === false && " · deregistered"}
          </p>
        </div>
        <div className="row">
          {row.new && <span className="badge new">NEW</span>}
          <EncBadge e={row.enc} />
          <StateBadge state={row.state} last={row.last} />
        </div>
      </section>
      <div className="kpis">
        <Stat label="Transmissions" value={compact(row.tx)} sub={dur(row.txSecs) + " on the air" + (row.encTx ? ` · ${row.encTx} encrypted` : "")} />
        {row.state !== undefined && (
          <Stat label="On / off" value={row.state === "off" ? "Off" : row.state === "on" ? "On" : "—"} sub={row.stateT ? `since ${clockAt(row.stateT)}` : "no registration heard"} />
        )}
        <Stat label="Last keyed up" value={row.lastTx ? ago(row.lastTx) : "never"} />
        <Stat
          label="Affiliated with"
          value={row.aff !== null ? affTag || row.aff : "—"}
          sub={row.affT ? `since ${clockAt(row.affT)}` : "none heard"}
          onClick={row.aff !== null ? () => setView("radio", nav, "tg", row.aff!) : undefined}
        />
        <Stat label="Talks with" value={partners.length} sub="radios" />
      </div>
      <div className="columns">
        <Card title="Talkgroups and radios">
          {tgs.length + partners.length === 0 ? (
            <p className="empty">Not heard transmitting yet.</p>
          ) : (
            <RadialEgo
              center={{ label: alias ? alias.slice(0, 12) : String(unit), sub: alias ? String(unit) : undefined }}
              inner={tgs.map((t) => ({ id: `t${t.tg}`, label: t.alphaTag || String(t.tg), weight: (t.talked ?? t.n) + (t.joins ?? 0) + (t.locations ?? 0), color: encrypted(t.enc) ? NET_ENC : undefined, sub: `${t.talked ?? t.n} calls, ${t.joins ?? 0} joins, ${t.locations ?? 0} locations · ${netWhy(false, false, t.enc, t.encCalls, t.talked)}` }))}
              outer={partners.map((p) => ({ id: `u${p.unit}`, label: aliasOf(s, sys.name, p.unit) || p.alias || String(p.unit), weight: p.n, color: encrypted(p.enc) ? NET_ENC : undefined, sub: `${p.n} calls together · ${netWhy(false, false, p.enc)}` }))}
              onClick={(n) => (n.id.startsWith("t") ? setView("radio", nav, "tg", n.id.slice(1)) : setView("radio", nav, "unit", n.id.slice(1)))}
            />
          )}
        </Card>
        <div className="stack">
          <Card title="Latest transmissions">
            <ul className="timeline">
              {(r.recent ?? []).map((t, i) => (
                <li key={i}>
                  <span className="muted small">{clockAt(t.t, true)}</span>
                  <button className="linkish" onClick={() => setView("radio", nav, "tg", t.tg)}>
                    {t.alphaTag || t.tg}
                  </button>
                  <span className="mono small">{num(t.secs, 1)} s</span>
                </li>
              ))}
            </ul>
          </Card>
          {tgs.some((t) => t.joins !== undefined) && <UnitTgs sys={sys} tgs={tgs} />}
          {r.events ? (
            <Card title="On / off, joins and locations">
              {r.events.length === 0 ? (
                <p className="empty">None heard.</p>
              ) : (
                <ul className="timeline">
                  {r.events.slice(0, 80).map((e, i) => (
                    <li key={i}>
                      <span className="muted small">{clockAt(e.t, true)}</span>
                      <span className={`badge ${e.kind === "off" ? "state-off" : e.kind === "on" ? "state-on" : "ign"}`}>{EVENT_LABEL[e.kind]}</span>
                      {e.tg !== null && (
                        <button className="linkish" onClick={() => setView("radio", nav, "tg", e.tg!)}>
                          {e.alphaTag || e.tg}
                        </button>
                      )}
                      {sc.multi && e.site && <span className="muted small">{e.site}</span>}
                    </li>
                  ))}
                </ul>
              )}
            </Card>
          ) : (
          <Card title="Affiliations">
            {(r.affiliations ?? []).length === 0 ? (
              <p className="empty">None heard.</p>
            ) : (
              <ul className="timeline">
                {(r.affiliations ?? []).map((a, i) => (
                  <li key={i}>
                    <span className="muted small">{clockAt(a.t, true)}</span>
                    <button className="linkish" onClick={() => setView("radio", nav, "tg", a.tg)}>
                      {a.alphaTag || a.tg}
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </Card>
          )}
          {(r.aliases ?? []).length > 1 && (
            <Card title="Aliases heard">
              <ul className="timeline">
                {r.aliases!.map((a) => (
                  <li key={a.alias}>
                    <span className="muted small">{clockAt(a.last, true)}</span>
                    <b>{a.alias}</b>
                  </li>
                ))}
              </ul>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
}

/** A talkgroup's radios by how they met it: began calls, talked, joined, location (radio database). */
function Links({ s, sys, links }: { s: AppState; sys: DashSystem; links?: LinkRow[] }) {
  const nav = useNav();
  if (!links) return null;
  return (
    <Card title={`Radios linked (${links.length})`}>
      {links.length === 0 ? (
        <p className="empty">None yet.</p>
      ) : (
        <div className="table-wrap tall">
          <table className="calls clickable-rows">
            <thead>
              <tr>
                <th>Radio</th>
                <th title="Calls it began">Began</th>
                <th title="Calls it talked on">Talked</th>
                <th title="Affiliations">Joined</th>
                <th title="Location registrations">Location</th>
                <th>Last</th>
              </tr>
            </thead>
            <tbody>
              {links.map((l) => (
                <tr key={l.unit} onClick={() => setView("radio", nav, "unit", l.unit)}>
                  <td>
                    <span className="sys-dot" style={{ background: stale(l.unitLast, l.state) ? NET_STALE : encrypted(l.enc) ? NET_ENC : "var(--series-3)" }} title={netWhy(stale(l.unitLast, l.state), l.state === "off", l.enc, l.encCalls, l.talked)} />
                    {aliasOf(s, sys.name, l.unit) ?? (l.alias || l.unit)}
                    <EncBadge e={l.enc} />
                  </td>
                  <td className="mono">{l.started || ""}</td>
                  <td className="mono">{l.talked || ""}</td>
                  <td className="mono">{l.joins || ""}</td>
                  <td className="mono">{l.locations || ""}</td>
                  <td className="small">{ago(l.last)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}

/** A radio's talkgroups by how it met them (radio database). */
function UnitTgs({ sys, tgs }: { sys: DashSystem; tgs: NonNullable<RadioResult["tgs"]> }) {
  const nav = useNav();
  return (
    <Card title="Talkgroups">
      <div className="table-wrap">
        <table className="calls clickable-rows">
          <thead>
            <tr>
              <th>Talkgroup</th>
              <th title="Calls it began">Began</th>
              <th title="Calls it talked on">Talked</th>
              <th title="Affiliations">Joined</th>
              <th title="Location registrations">Location</th>
              <th>Last</th>
            </tr>
          </thead>
          <tbody>
            {tgs.map((t) => (
              <tr key={t.tg} onClick={() => setView("radio", nav, "tg", t.tg)}>
                <td>
                  <TgName r={t} />
                  <EncBadge e={t.enc} />
                </td>
                <td className="mono">{t.started || ""}</td>
                <td className="mono">{t.talked || ""}</td>
                <td className="mono">{t.joins || ""}</td>
                <td className="mono">{t.locations || ""}</td>
                <td className="small">{t.last ? ago(t.last) : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

// ── the page ─────────────────────────────────────────────────────────────────

/** Which system a page is about: its colour, name, kind and site (or its sites), and the way back to all of them. */
function SystemHeader({ s, sys, sc, multi }: { s: AppState; sys: DashSystem; sc: Scope; multi: boolean }) {
  const [editing, setEditing] = useState<string | null>(null);
  const id = sys.status?.identity;
  const site = id && (id.sysId != null || id.site != null) ? [id.sysId != null ? `SysID ${id.sysId.toString(16).toUpperCase()}` : "", id.site != null ? `site ${id.rfss ?? "?"}-${id.site}` : ""].filter(Boolean).join(" · ") : "";
  const key = sc.key;
  const save = (v: string) => {
    updateConfig((c) => {
      const names = { ...(c.systemNames ?? {}) };
      if (v.trim()) names[key!] = v.trim();
      else delete names[key!];
      c.systemNames = names;
    });
    setEditing(null);
  };
  return (
    <div className="sys-header" style={{ borderLeftColor: systemColor(s.config, sys.name) }}>
      <div>
        {sc.all && editing !== null ? (
          <form
            className="row"
            onSubmit={(e) => {
              e.preventDefault();
              save(editing);
            }}
          >
            <input className="search" autoFocus value={editing} placeholder={systemLabel(key!, null)} onChange={(e) => setEditing(e.target.value)} />
            <button className="btn small">Save</button>
            <button type="button" className="btn ghost small" onClick={() => setEditing(null)}>
              Cancel
            </button>
          </form>
        ) : (
          <h2 className="big">
            {sc.all ? sc.label : sys.name}
            {sc.all && (
              <button className="btn ghost small" style={{ marginLeft: 12, verticalAlign: "middle" }} onClick={() => setEditing(s.config?.systemNames?.[key!] ?? "")} title="Name this system (blank: its WACN and System ID)">
                Rename
              </button>
            )}
          </h2>
        )}
        <p className="muted small">
          {KIND_LABEL[sys.kind]}
          {sc.all ? ` · ${keyText(key!)} · ${sc.sites.length} sites: ${sc.sites.join(", ")}` : site ? ` · ${site}` : ""}
          {!sc.all && sc.multi && key && (
            <>
              {" · a site of "}
              <button className="linkish" onClick={() => setView("radio", key)}>
                {systemLabel(key, s.config)}
              </button>
            </>
          )}
        </p>
      </div>
      {multi && (
        <button className="btn ghost small" onClick={() => setView("radio")}>
          All systems
        </button>
      )}
    </div>
  );
}

/** The system each site is of, when the radio database has seen several of its sites. */
function groupsOf(systems: DashSystem[], sum: RadioResult | null) {
  return (x: DashSystem) => {
    const k = sum?.systems?.[x.name]?.systemKey;
    return k && sum?.systems?.[k] && systems.filter((y) => sum?.systems?.[y.name]?.systemKey === k).length > 1 ? k : undefined;
  };
}

/** Which system or site the page shows, one row: a system with several sites gets "All" (the system) before its sites. */
function SitePicker({ s, sys, systems, sc, sum }: { s: AppState; sys: DashSystem; systems: DashSystem[]; sc: Scope; sum: RadioResult | null }) {
  const groupOf = groupsOf(systems, sum);
  const groups = new Set(systems.map(groupOf).filter(Boolean));
  if (systems.length < 2) return null;
  const seen = new Set<string>();
  return (
    <div className="filter-row" role="tablist" aria-label="System">
      {systems.map((x) => {
        const k = groupOf(x);
        const first = k && !seen.has(k);
        if (k) seen.add(k);
        return (
          <Fragment key={x.name}>
            {first && (
              <button className={`btn ghost small${sc.all && sc.key === k ? " on" : ""}`} onClick={() => setView("radio", k)} title={`${systemLabel(k, s.config)}: every site, each call once`}>
                {groups.size > 1 ? `All ${systemLabel(k, s.config)}` : "All"}
              </button>
            )}
            <button className={`btn ghost small${x.name === sys.name && !sc.all ? " on" : ""}`} onClick={() => setView("radio", x.name)}>
              <span className="sys-dot" style={{ background: systemColor(s.config, x.name) }} />
              {x.name}
            </button>
          </Fragment>
        );
      })}
    </div>
  );
}

/** Several systems: each one separately, side by side — never added together. Sites of one system share a card. */
function AllSystems({ s, systems, sum }: { s: AppState; systems: DashSystem[]; sum: RadioResult | null }) {
  const live = running(s);
  const groupOf = groupsOf(systems, sum);
  const cards: { query: string; title: string; sites: DashSystem[] }[] = [];
  for (const x of systems) {
    const combined = groupOf(x);
    const found = combined ? cards.find((c) => c.query === combined) : undefined;
    if (found) found.sites.push(x);
    else cards.push({ query: combined ?? x.name, title: combined ? systemLabel(combined, s.config) : x.name, sites: [x] });
  }
  const ids = cards.map((c) => c.query).join(",");
  const tops = usePolled(`tops:${ids}`, () => Promise.all(cards.map((c) => radioQuery({ what: "talkgroups", system: c.query, hours: 24, limit: 5 }))), live ? 60_000 : 0);
  const open = (c: (typeof cards)[number], what?: "tg", key?: number) => {
    if (what && key !== undefined) setView("radio", c.query, what, key);
    else setView("radio", c.query);
  };
  return (
    <div className="card-grid wide">
      {cards.map((c, i) => {
        const x = c.sites[0];
        const siteNames = c.sites.map((y) => y.name);
        const sm = sum?.systems?.[c.query];
        const top = (tops?.[i]?.rows ?? []) as TgRow[];
        const onAir = s.calls.filter((call) => siteNames.includes(call.systemName));
        return (
          <Card
            key={c.query}
            className="sys-section"
            title={
              <span className="row">
                <span className="sys-dot" style={{ background: systemColor(s.config, x.name) }} />
                {c.title}
                {c.sites.length > 1 && <span className="muted small">{siteNames.join(" · ")}</span>}
              </span>
            }
            onTitle={() => open(c)}
            actions={<span className="badge-kind">{c.sites.length > 1 ? `${KIND_LABEL[x.kind]} · ${c.sites.length} sites` : KIND_LABEL[x.kind]}</span>}
          >
            <div className="mini-stats">
              <div>
                <b>{sm ? compact(sm.tgs24h) : "—"}</b>
                <span>talkgroups, 24 h</span>
              </div>
              <div>
                <b>{sm ? compact(sm.units24h) : "—"}</b>
                <span>radios, 24 h</span>
              </div>
              <div>
                <b className={sm?.newTgs.length ? "accent" : ""}>{sm ? sm.newTgs.length : "—"}</b>
                <span>new talkgroups</span>
              </div>
              <div>
                <b>{sm ? sm.unknownTgs24h : "—"}</b>
                <span>not in its file</span>
              </div>
            </div>
            {top.length > 0 ? (
              <ul className="barlist">
                {top.map((r) => (
                  <li key={r.tg}>
                    <button className="linkish" onClick={() => open(c, "tg", r.tg)}>
                      <TgName r={r} />
                    </button>
                    <span className="bar" style={{ width: `${(r.secs / Math.max(1, top[0].secs)) * 100}%`, background: systemColor(s.config, x.name) }} />
                    <b className="mono">{r.calls}</b>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="empty">Nothing heard in the last day.</p>
            )}
            {onAir.length > 0 && (
              <div className="row tg-chips">
                {[...new Map(onAir.map((call) => [call.talkgroup, call])).values()].slice(0, 6).map((call) => (
                  <button key={call.talkgroup} className="chip" onClick={() => open(c, "tg", call.talkgroup)}>
                    <span className={`dot dot-${call.state}`} />
                    {call.alphaTag || call.talkgroup}
                  </button>
                ))}
              </div>
            )}
            {sm && sm.newTgs.length > 0 && (
              <p className="small muted">
                New: {sm.newTgs.slice(0, 6).map((t) => t.alphaTag || t.tg).join(", ")}
                {sm.newTgs.length > 6 ? "…" : ""}
              </p>
            )}
            <button className="card-more" onClick={() => open(c)}>
              Open {c.title} ▸
            </button>
          </Card>
        );
      })}
    </div>
  );
}

export function RadioPage() {
  const s = useApp();
  const systems = dashSystems(s);
  const multi = systems.length > 1;
  const sum = usePolled("radio-summary", () => radioQuery({ what: "summary" }), running(s) ? 60_000 : 0);
  if (!systems.length) return <p className="empty">No systems set up yet.</p>;
  // Several systems and none picked: each on its own.
  if (!s.path[0] && multi) {
    return (
      <div className="page">
        {!running(s) && (
          <p className="muted small">
            <Light level="idle" /> Not recording — showing past data.
          </p>
        )}
        <AllSystems s={s} systems={systems} sum={sum} />
      </div>
    );
  }
  const name = s.path[0] ?? systems[0].name;
  const groupOf = groupsOf(systems, sum);
  // A system's key: all its sites together.
  const members = systems.filter((x) => groupOf(x) === name);
  let sys: DashSystem | undefined;
  let sc: Scope;
  if (members.length) {
    sys = members[0];
    sc = { query: name, all: true, multi: true, key: name, sites: members.map((x) => x.name), label: systemLabel(name, s.config) };
  } else {
    sys = systems.find((x) => x.name === name);
    if (!sys) return <p className="empty">{sum ? `No system called ${name}.` : "Loading…"}</p>;
    const k = groupOf(sys);
    sc = { query: sys.name, all: false, multi: !!k, key: k ?? sum?.systems?.[sys.name]?.systemKey, sites: k ? systems.filter((x) => groupOf(x) === k).map((x) => x.name) : [sys.name], label: sys.name };
  }
  const [, what, key] = s.path;
  return (
    <ScopeCtx.Provider value={sc}>
      <div className="page">
        <SystemHeader s={s} sys={sys} sc={sc} multi={multi} />
        <SitePicker s={s} sys={sys} systems={systems} sc={sc} sum={sum} />
        {!running(s) && (
          <p className="muted small">
            <Light level="idle" /> Not recording — showing past data.
          </p>
        )}
        {what === "tg" && key ? <TgView sc={sc} s={s} sys={sys} tg={Number(key)} /> : what === "unit" && key ? <UnitView sc={sc} s={s} sys={sys} unit={Number(key)} /> : <SystemView sc={sc} s={s} sys={sys} />}
      </div>
    </ScopeCtx.Provider>
  );
}
