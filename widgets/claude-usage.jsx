// Claude Max usage — 5h + weekly per account. Reads the usage cache via paos.
//
// Repointed from dash-claude-usage (Python) to `paos accounts raw` (Rust): same JSON
// contract — stale + accounts[] with slot/active/email/fiveHour/sevenDay/resetsAt —
// verified by parsing both against the live cache, structures identical. Not BYTE
// identical: Python's json.dumps writes ", " separators and the Rust is compact. A JSON
// consumer cannot tell, which is why the parsed comparison is the one that counts.
//
// This polls every 5 SECONDS, so a wrong path here is a visibly dead widget rather than a
// quiet one. That is also why it was the last thing standing between dash-claude-usage
// and deletion.
export const command = "$HOME/.local/bin/paosctl accounts raw";
export const refreshFrequency = 5000;

export const className = `
  bottom: 12px; left: 12px; width: 340px;
  font-family: -apple-system, system-ui, sans-serif; color: #e6e6e6;
  background: rgba(20,20,24,0.82); border-radius: 10px; padding: 12px 14px;
  box-shadow: 0 4px 18px rgba(0,0,0,0.35); font-size: 12px;
  .cu-h { font-weight: 700; font-size: 12px; letter-spacing: .04em; opacity: .8; margin-bottom: 8px; }
  .cu-row { margin-bottom: 10px; }
  .cu-name { display:flex; justify-content:space-between; margin-bottom: 3px; }
  .cu-email { opacity: .85; } .cu-active { color:#6ee7a8; font-weight:700; }
  .cu-bar { height: 6px; border-radius: 4px; background: rgba(255,255,255,.12); overflow:hidden; margin: 2px 0; }
  .cu-fill { height: 100%; }
  .cu-lab { display:flex; justify-content:space-between; opacity:.7; font-size: 10px; }
  .cu-reset { opacity: .55; }
  .cu-stale { opacity:.6; font-style: italic; }
`;

const color = (u) => u == null ? "#888" : u >= 95 ? "#ef4444" : u >= 90 ? "#f59e0b" : u >= 75 ? "#eab308" : "#22c55e";
const pct = (u) => u == null ? "–" : Math.round(u) + "%";

// "resets in 1h20m" / "4d3h" / "12m" from an ISO timestamp
const resetTxt = (iso) => {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (isNaN(t)) return "";
  let s = Math.max(0, Math.floor((t - Date.now()) / 1000));
  const d = Math.floor(s / 86400); s %= 86400;
  const h = Math.floor(s / 3600); s %= 3600;
  const m = Math.floor(s / 60);
  const span = d > 0 ? `${d}d${h}h` : h > 0 ? `${h}h${m}m` : `${m}m`;
  return `resets in ${span}`;
};

const bar = (u, label, resetsAt) => (
  <div>
    <div className="cu-lab">
      <span>{label}</span>
      <span>{pct(u)}<span className="cu-reset">{resetsAt ? `  ·  ${resetTxt(resetsAt)}` : ""}</span></span>
    </div>
    <div className="cu-bar"><div className="cu-fill" style={{ width: (u == null ? 0 : Math.min(u,100)) + "%", background: color(u) }} /></div>
  </div>
);

export const render = ({ output }) => {
  let d;
  try { d = JSON.parse(output); } catch (e) { return <div className="cu-stale">Claude usage: loading…</div>; }
  if (d.error) return <div className="cu-stale">Claude usage: {String(d.error)}</div>;
  const accts = [...(d.accounts || [])].sort((a, b) => (b.active ? 1 : 0) - (a.active ? 1 : 0));
  if (!accts.length) return <div className="cu-stale">No accounts captured. Run: paos accounts capture</div>;
  return (
    <div>
      <div className="cu-h">CLAUDE MAX USAGE {d.stale ? "· stale" : ""}</div>
      {accts.map((a) => (
        <div className="cu-row" key={a.slot}>
          <div className="cu-name">
            <span className={a.active ? "cu-active" : "cu-email"}>{a.active ? "● " : ""}{a.email || a.slot}</span>
            {a.error ? <span style={{ color: "#ef4444" }}>err</span> : null}
          </div>
          {a.error ? null : bar((a.fiveHour || {}).util, "5h", (a.fiveHour || {}).resetsAt)}
          {a.error ? null : bar((a.sevenDay || {}).util, "week", (a.sevenDay || {}).resetsAt)}
          {(a.sevenDayOpus && a.sevenDayOpus.util != null) ? bar(a.sevenDayOpus.util, "week · opus", a.sevenDayOpus.resetsAt) : null}
        </div>
      ))}
    </div>
  );
};
