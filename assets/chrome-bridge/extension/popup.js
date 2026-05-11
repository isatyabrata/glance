chrome.runtime.sendMessage({ kind: "status" }, (resp) => {
  const el = document.getElementById("status");
  if (chrome.runtime.lastError || !resp) {
    el.innerHTML = '<span class="bad">SW not awake</span>';
    return;
  }
  el.innerHTML = resp.connected
    ? `<span class="ok">Connected to glance</span><div class="row">host: ${resp.host}</div>`
    : `<span class="bad">Not connected</span><div class="row">${resp.error || "host unreachable"}</div>`;
});
