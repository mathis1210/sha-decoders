"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
const ALGS = ["sha1", "sha256", "sha384", "sha512"];

// ---------------------------------------------------------------- format ---

const fmtInt = (n) => (n || 0).toLocaleString("en-US");

function fmtBytes(b) {
  b = b || 0;
  if (b < 1000) return b + " B";
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let v = b / 1000;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return v.toFixed(v < 10 ? 2 : 1) + " " + units[i];
}

function fmtRate(n) {
  n = n || 0;
  if (n >= 1e9) return (n / 1e9).toFixed(2) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(2) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return String(Math.round(n));
}

function textLabel(s) {
  return typeof s === "string" ? '"' + s + '"' : "—";
}

// ---------------------------------------------------------------- status ---

let lastStatus = null;
let limitsFilled = false;

function render(data) {
  if (!data) return;
  lastStatus = data;

  const running = !!data.running;
  const pill = $("#status-pill");
  pill.classList.toggle("is-running", running);
  const stateText = $("#status-text");
  if (running) {
    stateText.textContent = "running · " + fmtRate(data.rate) + "/s";
  } else if (data.stopped_by) {
    stateText.textContent = "stopped";
  } else {
    stateText.textContent = "paused";
  }

  $("#btn-start").disabled = running;
  $("#btn-stop").disabled = !running;

  const reason = $("#stop-reason");
  if (!running && data.stopped_by) {
    reason.textContent = "Stopped: " + data.stopped_by + ".";
    reason.classList.remove("hidden");
  } else {
    reason.classList.add("hidden");
  }

  $("#stat-total").textContent = fmtInt(data.total);
  $("#stat-total-note").textContent =
    data.total > 0 ? "across " + ALGS.filter((a) => data.entries[a] > 0).length + " databases" : "across four databases";

  $("#stat-bytes").textContent = fmtBytes(data.size_total);
  $("#stat-bytes-note").textContent =
    data.total > 0 ? (data.size_total / data.total).toFixed(2) + " bytes per entry" : "compressed records";

  $("#stat-rate").textContent = running ? fmtRate(data.rate) + "/s" : "0";
  $("#stat-rate-note").textContent = running
    ? fmtBytes(data.write_bps) + "/s written"
    : "idle";

  $("#stat-next").textContent = textLabel(data.next_text);
  $("#stat-next-note").textContent =
    data.next_index !== undefined ? "index " + fmtInt(data.next_index) : "resume point";

  $("#db-meta").textContent = fmtInt(data.total) + " entries · " + fmtBytes(data.size_total);

  for (const alg of ALGS) {
    const row = document.querySelector('[data-row="' + alg + '"]');
    if (!row) continue;
    row.classList.toggle("is-off", !data.enabled[alg]);
    row.querySelector('[data-count="' + alg + '"]').textContent = fmtInt(data.entries[alg]);
    row.querySelector('[data-size="' + alg + '"]').textContent = fmtBytes(data.sizes[alg]);
    const toggle = row.querySelector('[data-toggle="' + alg + '"]');
    if (document.activeElement !== toggle) toggle.checked = !!data.enabled[alg];
  }

  $("#limits-note").textContent = data.limits_text || "no stop rule set";

  if (!limitsFilled) {
    const l = data.limits || {};
    $("#limit-storage").value = l.storage_bytes ? (l.storage_bytes / 1e9).toString() : "";
    $("#limit-per-db").value = l.per_db ? String(l.per_db) : "";
    $("#limit-total").value = l.total_entries ? String(l.total_entries) : "";
    limitsFilled = true;
  }
}

async function refresh() {
  try {
    const r = await fetch("/api/status", { cache: "no-store" });
    if (r.ok) render(await r.json());
  } catch (_) {
    /* server unreachable; leave the last numbers on screen */
  }
}

// --------------------------------------------------------------- live feed ---

let source = null;
let poller = null;

function setLive(live) {
  const pill = $("#status-pill");
  pill.classList.toggle("is-stale", !live);
  if (!live && !lastStatus) $("#status-text").textContent = "reconnecting…";
}

function startPolling() {
  if (poller) return;
  poller = setInterval(refresh, 1200);
}
function stopPolling() {
  if (poller) {
    clearInterval(poller);
    poller = null;
  }
}

function connect() {
  if (!("EventSource" in window)) {
    startPolling();
    return;
  }
  if (source) source.close();
  source = new EventSource("/api/stream");
  source.onopen = () => {
    setLive(true);
    stopPolling();
  };
  source.onmessage = (ev) => {
    try {
      render(JSON.parse(ev.data));
      setLive(true);
    } catch (_) {
      /* ignore a torn frame */
    }
  };
  source.onerror = () => {
    setLive(false);
    startPolling();
  };
}

// ------------------------------------------------------------------ posts ---

async function post(path, body) {
  const opts = { method: "POST" };
  if (body !== undefined) opts.body = body;
  const r = await fetch(path, opts);
  let data = null;
  try {
    data = await r.json();
  } catch (_) {
    data = null;
  }
  if (!r.ok || (data && data.ok === false)) {
    throw new Error((data && data.error) || "request failed (" + r.status + ")");
  }
  return data;
}

function flash(el, text, ms = 2200) {
  el.textContent = text;
  if (flash.timer) clearTimeout(flash.timer);
  flash.timer = setTimeout(() => {
    el.textContent = "";
  }, ms);
}

// ---------------------------------------------------------------- controls ---

$("#btn-start").addEventListener("click", async () => {
  const btn = $("#btn-start");
  btn.disabled = true;
  try {
    render(await post("/api/start"));
  } catch (e) {
    $("#stop-reason").textContent = "Could not start: " + e.message;
    $("#stop-reason").classList.remove("hidden");
    btn.disabled = false;
  }
});

$("#btn-stop").addEventListener("click", async () => {
  const btn = $("#btn-stop");
  btn.disabled = true;
  try {
    render(await post("/api/stop"));
  } catch (e) {
    btn.disabled = false;
  }
});

for (const alg of ALGS) {
  const row = document.querySelector('[data-row="' + alg + '"]');
  if (!row) continue;

  row.querySelector('[data-toggle="' + alg + '"]').addEventListener("change", async (ev) => {
    const on = ev.target.checked;
    try {
      render(await post("/api/toggle", "alg=" + alg + "&on=" + (on ? "1" : "0")));
    } catch (e) {
      ev.target.checked = !on;
      $("#limits-note").textContent = "Could not switch " + alg + ": " + e.message;
    }
  });

  row.querySelector('[data-remove="' + alg + '"]').addEventListener("click", async () => {
    const ok = window.confirm(
      "Delete all " + alg + " data? Its database is removed and the algorithm switches off. This cannot be undone."
    );
    if (!ok) return;
    try {
      render(await post("/api/remove", "alg=" + alg));
      flash($("#limits-note"), alg + " deleted");
    } catch (e) {
      $("#limits-note").textContent = "Could not delete " + alg + ": " + e.message;
    }
  });
}

$("#limits-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const body =
    "storage_gb=" + encodeURIComponent($("#limit-storage").value.trim() || "0") +
    "&per_db=" + encodeURIComponent($("#limit-per-db").value.trim() || "0") +
    "&total_entries=" + encodeURIComponent($("#limit-total").value.trim() || "0");
  try {
    render(await post("/api/limits", body));
    flash($("#limits-msg"), "rules saved");
  } catch (e) {
    $("#limits-msg").textContent = "Could not save: " + e.message;
  }
});

$("#btn-clear").addEventListener("click", async () => {
  const ok = window.confirm(
    "Empty all four databases? Every stored entry is deleted and the sequence restarts at \"a\"."
  );
  if (!ok) return;
  try {
    render(await post("/api/clear"));
    $("#hash-list").innerHTML = "";
    $("#decode-result").classList.add("hidden");
    flash($("#limits-msg"), "databases emptied");
  } catch (e) {
    $("#limits-msg").textContent = "Could not empty: " + e.message;
  }
});

// -------------------------------------------------------------------- tabs ---

$$(".tab").forEach((tab) => {
  tab.addEventListener("click", () => {
    const name = tab.dataset.tab;
    $$(".tab").forEach((t) => {
      const on = t === tab;
      t.classList.toggle("active", on);
      t.setAttribute("aria-selected", on ? "true" : "false");
    });
    $("#panel-encode").classList.toggle("hidden", name !== "encode");
    $("#panel-decode").classList.toggle("hidden", name !== "decode");
    (name === "encode" ? $("#encode-input") : $("#decode-input")).focus();
  });
});

// ----------------------------------------------------------------- encode ---

function hashRow(alg, hex) {
  const li = document.createElement("li");
  li.className = "hash-item";
  const label = document.createElement("span");
  label.className = "hash-alg";
  label.textContent = alg;
  const value = document.createElement("span");
  value.className = "hash-value";
  value.textContent = hex;
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "copy";
  copy.textContent = "copy";
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(hex);
      copy.textContent = "copied";
    } catch (_) {
      const r = document.createRange();
      r.selectNodeContents(value);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(r);
      copy.textContent = "selected";
    }
    setTimeout(() => (copy.textContent = "copy"), 1600);
  });
  li.append(label, value, copy);
  return li;
}

$("#btn-encode").addEventListener("click", async () => {
  const input = $("#encode-input");
  const text = input.value;
  const msg = $("#encode-msg");
  if (!text.length) {
    msg.textContent = "Type something to hash first.";
    return;
  }
  try {
    const data = await post("/api/encode", text);
    const list = $("#hash-list");
    list.innerHTML = "";
    for (const alg of ALGS) if (data[alg]) list.append(hashRow(alg, data[alg]));
    msg.textContent = data.saved === false ? "hashed (not saved: all databases off)" : "saved to every enabled database";
    refresh();
  } catch (e) {
    msg.textContent = "Could not hash: " + e.message;
  }
});

// ----------------------------------------------------------------- decode ---

$("#btn-decode").addEventListener("click", async () => {
  const input = $("#decode-input");
  const hex = input.value.trim().toLowerCase();
  const msg = $("#decode-msg");
  const out = $("#decode-result");
  out.classList.add("hidden");
  if (!/^[0-9a-f]+$/.test(hex) || [40, 64, 96, 128].indexOf(hex.length) === -1) {
    msg.textContent = "Expected 40, 64, 96 or 128 hex characters.";
    return;
  }
  try {
    const data = await post("/api/decode", hex);
    if (data.found) {
      out.innerHTML = "";
      const label = document.createElement("span");
      label.className = "result-label";
      label.textContent = data.alg + " · match";
      const text = document.createElement("span");
      text.className = "result-text";
      text.textContent = data.text;
      out.classList.remove("is-miss");
      out.append(label, text);
      out.classList.remove("hidden");
      msg.textContent = "";
    } else {
      out.innerHTML = "";
      const label = document.createElement("span");
      label.className = "result-label";
      label.textContent = data.alg + " · no match";
      const text = document.createElement("span");
      text.className = "result-text";
      text.textContent = "That hash is not in this database yet.";
      out.classList.add("is-miss");
      out.append(label, text);
      out.classList.remove("hidden");
      msg.textContent = "";
    }
  } catch (e) {
    msg.textContent = "Lookup failed: " + e.message;
  }
});

$("#decode-input").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") $("#btn-decode").click();
});
$("#encode-input").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter" && (ev.metaKey || ev.ctrlKey)) $("#btn-encode").click();
});

// -------------------------------------------------------------- crossovers ---

function crossItem(head, main, sub, kind, badge, hashes) {
  const li = document.createElement("li");
  li.className = "cross-item" + (kind ? " is-" + kind : "");
  const h = document.createElement("span");
  h.className = "cross-head";
  h.textContent = head;
  const m = document.createElement("span");
  m.className = "cross-main";
  m.textContent = main;
  li.append(h, m);
  if (badge) {
    const b = document.createElement("span");
    b.className = "cross-badge " + badge.cls;
    b.textContent = badge.text;
    li.append(b);
  }
  if (sub) {
    const s = document.createElement("span");
    s.className = "cross-sub";
    s.textContent = sub;
    li.append(s);
  }
  if (hashes) {
    for (const line of hashes) {
      const d = document.createElement("span");
      d.className = "cross-hash";
      d.textContent = line;
      li.append(d);
    }
  }
  return li;
}

function crossCell(label, text, cls) {
  const c = document.createElement("span");
  c.className = "cross-cell" + (cls ? " " + cls : "");
  c.dataset.label = label;
  c.textContent = text;
  return c;
}

function crossHeadRow() {
  const row = document.createElement("div");
  row.className = "cross-row cross-row-head";
  for (const t of ["database", "entries", "key", "hash", "match"]) {
    const s = document.createElement("span");
    s.textContent = t;
    row.append(s);
  }
  return row;
}

function crossEmpty(text) {
  const d = document.createElement("div");
  d.className = "cross-row cross-empty";
  d.textContent = text;
  return d;
}

function crossRow(e) {
  const row = document.createElement("div");
  row.className = "cross-row";

  row.append(crossCell("database", e.alg + " · shard " + e.shard, "mono"));

  const pair = crossCell("entries", e.aText + "  ↔  " + e.bText, "mono cross-pair");
  const idx = document.createElement("span");
  idx.className = "cross-sub";
  idx.textContent =
    "indices " + e.aIndex.toLocaleString("en-US") + " and " + e.bIndex.toLocaleString("en-US");
  pair.append(idx);
  row.append(pair);

  row.append(crossCell("key", e.key, "mono"));

  const hash = crossCell("hash", "", "mono");
  for (const line of e.sameHash ? [e.aHash] : ["a " + e.aHash, "b " + e.bHash]) {
    const l = document.createElement("span");
    l.className = "cross-hash-line";
    l.textContent = line;
    hash.append(l);
  }
  row.append(hash);

  const badge = document.createElement("span");
  badge.className = "cross-badge " + (e.sameHash ? "is-same" : "is-key");
  badge.textContent = e.sameHash ? "same hash" : "key only";
  row.append(badge);

  return row;
}

$("#btn-cross-scan").addEventListener("click", async () => {
  const btn = $("#btn-cross-scan");
  const msg = $("#cross-msg");
  const list = $("#cross-list");
  const which = $("#cross-alg").value;
  btn.disabled = true;
  list.innerHTML = "";
  msg.textContent = "scanning " + which + " — reads the whole database once";
  try {
    const data = await post("/api/crossover/scan", "alg=" + encodeURIComponent(which));
    const secs = (data.took_ms / 1000).toFixed(1);
    if (data.found === 0) {
      msg.textContent = "No shared keys in " + which + " (" + secs + "s).";
      list.append(crossEmpty("No crossovers — every shard key is unique in this database."));
      return;
    }
    msg.textContent =
      data.found +
      " crossover" +
      (data.found === 1 ? "" : "s") +
      " in " +
      which +
      (data.sameHash ? " · " + data.sameHash + " with an identical hash" : " · all key only") +
      " (" +
      secs +
      "s).";
    list.append(crossHeadRow());
    for (const e of data.examples) list.append(crossRow(e));
  } catch (err) {
    msg.textContent = "Scan failed: " + err.message;
    list.append(crossEmpty("Scan failed — " + err.message));
  } finally {
    btn.disabled = false;
  }
});

$("#btn-rand-check").addEventListener("click", async () => {
  const btn = $("#btn-rand-check");
  const msg = $("#rand-msg");
  const list = $("#rand-list");
  const count = Math.max(1, Math.min(2000, Number($("#rand-count").value) || 50));
  const size = Math.max(1, Math.min(256, Number($("#rand-size").value) || 16));
  btn.disabled = true;
  list.innerHTML = "";
  msg.textContent = "hashing and checking — each file reads one shard per algorithm";
  try {
    const data = await post("/api/crossover/random", "count=" + count + "&size=" + size);
    const secs = (data.took_ms / 1000).toFixed(1);
    const per = Object.entries(data.algs)
      .map(([k, v]) => k + " " + v.matches + "/" + v.checked)
      .join(" · ");
    msg.textContent = data.files + " files × " + data.size + " B in " + secs + "s · " + per;
    if (data.totalHits === 0) {
      list.append(
        crossItem("result", "No stored digest matched", "random data almost never coincides with a generated text", "miss")
      );
    } else {
      for (const m of data.matches) {
        list.append(
          crossItem(
            m.alg + " · file " + m.file + " · " + m.source,
            m.text,
            "data " + JSON.stringify(m.data) + " · index " + m.index,
            "hit"
          )
        );
      }
    }
  } catch (err) {
    msg.textContent = "Check failed: " + err.message;
  } finally {
    btn.disabled = false;
  }
});

// ------------------------------------------------------------------- boot ---

refresh();
connect();
