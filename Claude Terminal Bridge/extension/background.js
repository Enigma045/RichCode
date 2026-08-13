// background.js

let socket = null;
let reconnectTimer = null;
let reconnectAttempts = 0;
let isConnecting = false;
let sessionToken = "";

// Prevent duplicate attachment jobs caused by repeated websocket delivery,
// service-worker wakeups, or a terminal command being echoed twice.
let activeAttachmentJob = null;

// Messages waiting for a live, authenticated socket. Previously any
// claude_message relay attempted while the socket was merely CONNECTING
// (e.g. we just woke a torn-down MV3 service worker and kicked off a
// reconnect) was dropped with no trace and no retry - unlike the Rust
// side's own pending queue for the opposite direction. That's exactly the
// class of bug this whole project has been chasing, just on this side of
// the wire, and it can silently eat a one-shot signal like
// conversation_active or rekey_conversation just as easily as a chat
// message. Bounded so a long outage can't grow this without limit.
const MAX_PENDING_OUTGOING = 50;
let pendingOutgoing = [];

function flushPendingOutgoing() {
    if (!socket || socket.readyState !== WebSocket.OPEN || pendingOutgoing.length === 0) return;
    const toSend = pendingOutgoing;
    pendingOutgoing = [];
    for (const json of toSend) {
        try {
            socket.send(json);
        } catch (e) {
            // Socket died mid-flush; put the rest back and let the next
            // open/retry attempt pick up where we left off.
            pendingOutgoing.push(json);
        }
    }
}

function sendOrQueue(payload) {
    const json = JSON.stringify(payload);
    if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(json);
    } else {
        if (pendingOutgoing.length >= MAX_PENDING_OUTGOING) {
            pendingOutgoing.shift();
        }
        pendingOutgoing.push(json);
    }
}

// We no longer rely on in-memory maps because Service Workers sleep.
// We will query Chrome directly when we need to find Claude tabs.
chrome.storage.local.get(["sessionToken"], (res) => {
    if (res.sessionToken) {
        sessionToken = res.sessionToken;
        connectWebSocket();
    }
});

chrome.storage.onChanged.addListener((changes, area) => {
    if (area === "local" && changes.sessionToken) {
        sessionToken = changes.sessionToken.newValue;
        if (sessionToken) {
            connectWebSocket();
        } else {
            if (socket) {
                socket.close();
            }
        }
    }
});

function connectWebSocket() {
    if (!sessionToken || isConnecting || (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING))) {
        return;
    }

    isConnecting = true;
    console.log("Attempting to connect to Rust bridge...");

    socket = new WebSocket("ws://127.0.0.1:8765");

    socket.onopen = () => {
        console.log("WebSocket connected");
        isConnecting = false;
        
        // Send hello
        socket.send(JSON.stringify({
            type: "hello",
            client: "claude-extension",
            version: "1.0",
            token: sessionToken
        }));

        // Flush anything queued while we were disconnected/reconnecting.
        // Note: the Rust side won't treat us as authenticated until it sees
        // our Hello, but it holds anything that arrives before that (see
        // queued_before_auth in websocket.rs) rather than dropping it, so
        // sending immediately after Hello here is safe and doesn't need to
        // wait for hello_ack.
        flushPendingOutgoing();
    };

    socket.onmessage = (event) => {
        try {
            const data = JSON.parse(event.data);
            handleServerMessage(data);
        } catch (err) {
            console.error("Failed to parse message from Rust:", err);
        }
    };

    socket.onclose = () => {
        console.log("WebSocket disconnected");
        socket = null;
        isConnecting = false;
        scheduleReconnect();
    };

    socket.onerror = (err) => {
        console.error("WebSocket error:", err);
        isConnecting = false;
    };
}

function scheduleReconnect() {
    if (reconnectTimer) clearTimeout(reconnectTimer);
    
    // Exponential backoff, max 30 seconds
    const delay = Math.min(1000 * Math.pow(2, reconnectAttempts), 30000);
    reconnectAttempts++;
    
    console.log(`Scheduling reconnect in ${delay}ms`);
    reconnectTimer = setTimeout(connectWebSocket, delay);
}

// setTimeout above is a best-effort fast path only. Chrome tears down an
// MV3 service worker after ~30s of inactivity, and any timers scheduled
// before teardown - including the setTimeout in scheduleReconnect - die
// with it and never fire. If that happens while the socket is closed, the
// bridge stays dead until something else happens to wake the worker back
// up (e.g. activity on a claude.ai tab), with no automatic recovery in the
// meantime. chrome.alarms is the one mechanism in MV3 that Chrome
// guarantees will wake/respawn a fully torn-down worker, so it's used here
// as a periodic backstop independent of whether the worker survived.
// (Alarm periods are clamped to a 1-minute minimum in production, so this
// is a backstop, not a substitute for reconnecting promptly when the
// worker is already alive - that's still handled by onclose/onerror above.)
chrome.alarms.create('ws-reconnect-check', { periodInMinutes: 1 });
chrome.alarms.onAlarm.addListener((alarm) => {
    if (alarm.name === 'ws-reconnect-check') {
        if (sessionToken && (!socket || socket.readyState === WebSocket.CLOSED)) {
            console.log("Alarm fired: socket is down, reconnecting.");
            connectWebSocket();
        }
    }
});

async function handleServerMessage(msg) {
    if (msg.type === "hello_ack") {
        console.log("Rust bridge authenticated successfully.");
        reconnectAttempts = 0; // Reset backoff on successful auth
        chrome.runtime.sendMessage({ type: "bridge_status", connected: true });
        flushPendingOutgoing();
        
        // Tell active Claude tabs to resend their chat history so the terminal isn't blank
        chrome.tabs.query({ url: ["*://claude.ai/*", "*://*.claude.ai/*"] }).then(tabs => {
            tabs.forEach(t => chrome.tabs.sendMessage(t.id, { type: "resync_history" }).catch(() => {}));
        });
    } else if (msg.type === "send_message") {
        // Forward to the active Claude tab dynamically
        const activeTabId = await getSelectedClaudeTabId();
        if (activeTabId) {
            chrome.tabs.sendMessage(activeTabId, {
                type: "inject_message",
                content: msg.content
            });
        }
    } else if (msg.type === "attach_file") {
        await attachFileToClaude(msg.path, msg.prompt || "");
    } else if (msg.type === "ping") {
        socket.send(JSON.stringify({ type: "pong" }));
    } else if (msg.type === "error") {
        console.error("Server error:", msg.message);
        if (msg.message === "Invalid token") {
            chrome.storage.local.remove(["sessionToken"]);
            sessionToken = "";
            if (reconnectTimer) clearTimeout(reconnectTimer);
            reconnectAttempts = 0;
            console.log("Invalid token, stopped connection attempts.");
        }
    }
}

async function getSelectedClaudeTabId() {
    const tabs = await chrome.tabs.query({
        url: ["*://claude.ai/*", "*://*.claude.ai/*"],
        active: true,
        lastFocusedWindow: true
    });
    if (tabs.length > 0) return tabs[0].id;

    const fallback = await chrome.tabs.query({
        url: ["*://claude.ai/*", "*://*.claude.ai/*"]
    });
    return fallback.length > 0 ? fallback[0].id : null;
}

/**
 * Attach a local file to Claude using Chrome's DevTools Protocol.
 *
 * Normal page JavaScript cannot assign a local filesystem path to
 * <input type=file> because browsers intentionally block that operation.
 * The debugger API exposes DOM.setFileInputFiles, which is the supported
 * CDP mechanism for setting a file input's selected files.
 */
async function attachFileToClaude(filePath, prompt) {
    const tabId = await getSelectedClaudeTabId();
    if (!tabId) {
        console.error("attach_file: no active Claude tab");
        return;
    }

    const jobKey = `${tabId}|${filePath}`;
    if (activeAttachmentJob === jobKey) {
        console.warn("Ignoring duplicate attach request:", jobKey);
        return;
    }
    activeAttachmentJob = jobKey;

    const debuggee = { tabId };

    try {
        await chrome.debugger.attach(debuggee, "1.3");

        const documentResult = await chrome.debugger.sendCommand(
            debuggee,
            "DOM.getDocument",
            { depth: -1, pierce: true }
        );

        const rootNodeId = documentResult.root.nodeId;
        const query = await chrome.debugger.sendCommand(
            debuggee,
            "DOM.querySelectorAll",
            {
                nodeId: rootNodeId,
                selector: 'input[type="file"]'
            }
        );

        if (!query.nodeIds || query.nodeIds.length === 0) {
            throw new Error("Claude file-upload input was not found. Open a Claude conversation and try again.");
        }

        let lastError = null;
        let attached = false;

        for (const nodeId of query.nodeIds) {
            try {
                await chrome.debugger.sendCommand(
                    debuggee,
                    "DOM.setFileInputFiles",
                    {
                        nodeId,
                        files: [filePath]
                    }
                );

                // CDP normally causes Chromium to update the FileList and
                // notify the framework itself. Do NOT blindly dispatch both
                // input and change here: Claude's uploader can treat the extra
                // synthetic events as a second selection and show the same file
                // twice. We only use a single change fallback if the UI does not
                // react to the CDP selection shortly afterward.
                await new Promise(resolve => setTimeout(resolve, 250));
                const reacted = await chrome.debugger.sendCommand(
                    debuggee,
                    "Runtime.evaluate",
                    {
                        returnByValue: true,
                        expression: `(() => {
                            const inputs = Array.from(document.querySelectorAll('input[type=file]'));
                            const hasFile = inputs.some(i => i.files && i.files.length > 0);
                            const body = document.body?.innerText || '';
                            return hasFile || /attachment|uploaded|uploading|file/i.test(body) || !!document.querySelector('[data-testid*="attachment"], [aria-label*="Remove"]');
                        })()`
                    }
                );

                if (!reacted?.result?.value) {
                    await chrome.debugger.sendCommand(
                        debuggee,
                        "Runtime.evaluate",
                        {
                            expression: `(() => {
                                const input = Array.from(document.querySelectorAll('input[type=file]'))
                                    .find(i => i.files && i.files.length);
                                if (!input) return false;
                                input.dispatchEvent(new Event('change', {bubbles: true}));
                                return true;
                            })()`
                        }
                    );
                }

                attached = true;
                break;
            } catch (err) {
                lastError = err;
            }
        }

        if (!attached) {
            throw lastError || new Error("Unable to set Claude's file input.");
        }

        // Let Claude's upload UI receive the selected file before submitting
        // the prompt. The content script performs another short readiness
        // check, so this is deliberately conservative.
        await new Promise(resolve => setTimeout(resolve, 500));

        if (prompt) {
            try {
                await chrome.tabs.sendMessage(tabId, {
                    type: "inject_message",
                    content: prompt,
                    waitForAttachment: true
                });
            } catch (err) {
                console.error("attach_file: failed to submit prompt:", err);
            }
        }

        console.log("Attached file to Claude:", filePath);
    } catch (err) {
        console.error("attach_file failed:", err);
        try {
            await chrome.tabs.sendMessage(tabId, {
                type: "claude_message",
                message: {
                    type: "diagnostic",
                    content: `Automatic file attachment failed: ${err.message || err}`
                }
            });
        } catch (_) {}
    } finally {
        try {
            await chrome.debugger.detach(debuggee);
        } catch (_) {}
        if (activeAttachmentJob === jobKey) {
            activeAttachmentJob = null;
        }
    }
}

// Listen for messages from content scripts or popup
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
    // Any incoming message means the worker just woke up (possibly from a
    // full teardown) - a good, fast opportunity to notice the socket is
    // dead and reconnect immediately rather than waiting for the alarm
    // backstop's up-to-1-minute delay.
    if (sessionToken && (!socket || socket.readyState === WebSocket.CLOSED)) {
        connectWebSocket();
    }

    if (request.type === "claude_message") {
        // Relay to Rust. Previously this only sent when the socket was
        // already OPEN and just dropped the message otherwise - queue it
        // instead so it survives a reconnect (see sendOrQueue above).
        const m = request.message;
        if (m.type === "rekey_conversation") {
            // Different shape from a chat message - no role/content/status.
            sendOrQueue({
                type: "rekey_conversation",
                old_conversation_id: m.old_conversation_id,
                new_conversation_id: m.new_conversation_id
            });
        } else if (m.type === "conversation_active") {
            // Also a different shape - just an id, no role/content/status.
            sendOrQueue({
                type: "conversation_active",
                conversation_id: m.conversation_id
            });
        } else if (m.type === "diagnostic") {
            sendOrQueue({
                type: "diagnostic",
                message: m.content
            });
        } else {
            sendOrQueue({
                type: m.type,
                conversation_id: m.conversation_id,
                message_id: m.message_id,
                role: m.role,
                content: m.content,
                status: m.status
            });
        }
    } else if (request.type === "claude_tab_status") {
        if (request.connected) {
            chrome.action.setIcon({ path: "icons/icon_connected.png" }).catch(() => {});
            if (socket && socket.readyState === WebSocket.OPEN && sender && sender.tab && sender.tab.id) {
                chrome.tabs.sendMessage(sender.tab.id, { type: "resync_history" }).catch(() => {});
            }
        } else {
            chrome.action.setIcon({ path: "icons/icon_disconnected.png" }).catch(() => {});
        }
    } else if (request.type === "get_status") {
        chrome.tabs.query({ url: ["*://claude.ai/*", "*://*.claude.ai/*"] }).then(tabs => {
            sendResponse({
                bridgeConnected: socket && socket.readyState === WebSocket.OPEN,
                claudeTabs: tabs.map(t => ({ id: t.id, connected: true, url: t.url }))
            });
        });
        return true; // Indicate async response
    }
});
