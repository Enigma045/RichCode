// claude-content.js

class ClaudeAdapter {
    constructor() {
        this.observer = null;
        this.lastProcessedMessages = new Set();
        this.currentStreamingMessage = null;
        this.debounceTimer = null;

        // Track a stable conversation id separately from raw URL reads.
        // A brand-new chat has no UUID in its URL yet, so we track it under
        // a placeholder until claude.ai assigns the real one.
        this.conversationId = this.extractConversationId();
        this.urlCheckTimer = null;
    }

    // claude.ai chat URLs look like /chat/<uuid>.
    // Before the first message is sent, there's no uuid segment yet.
    extractConversationId() {
        const match = window.location.pathname.match(
            /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/i
        );

        return match ? match[1] : "pending";
    }

    // Poll for SPA conversation URL changes.
    checkForConversationIdChange() {
        const newId = this.extractConversationId();

        if (newId !== this.conversationId) {
            const oldId = this.conversationId;
            this.conversationId = newId;

            if (newId !== "pending") {
                chrome.runtime.sendMessage({
                    type: "claude_message",
                    message: {
                        type: "conversation_active",
                        conversation_id: newId
                    }
                });
            }

            if (oldId === "pending" && newId !== "pending") {
                chrome.runtime.sendMessage({
                    type: "claude_message",
                    message: {
                        type: "rekey_conversation",
                        old_conversation_id: oldId,
                        new_conversation_id: newId
                    }
                });
            }
        }
    }

    startObserving() {
        if (this.observer) return;

        console.log(
            "Claude Terminal Bridge: Starting observation..."
        );

        chrome.runtime.sendMessage({
            type: "claude_tab_status",
            connected: true
        });

        // Report current conversation immediately if one exists.
        if (this.conversationId !== "pending") {
            chrome.runtime.sendMessage({
                type: "claude_message",
                message: {
                    type: "conversation_active",
                    conversation_id: this.conversationId
                }
            });
        }

        this.observer = new MutationObserver((mutations) => {
            this.handleMutations(mutations);
        });

        this.observer.observe(document.body, {
            childList: true,
            subtree: true,
            characterData: true
        });

        if (!this.urlCheckTimer) {
            this.urlCheckTimer = setInterval(() => {
                this.checkForConversationIdChange();
            }, 500);
        }

        this.scanExistingMessages();
    }

    stopObserving() {
        if (this.observer) {
            this.observer.disconnect();
            this.observer = null;
        }

        if (this.urlCheckTimer) {
            clearInterval(this.urlCheckTimer);
            this.urlCheckTimer = null;
        }

        if (this.debounceTimer) {
            clearTimeout(this.debounceTimer);
            this.debounceTimer = null;
        }
    }

    getMessages() {
        const selectors = [
            "[data-message-author-role]",
            '[data-testid="user-message"]',
            '[data-testid="assistant-message"]',
            ".font-user-message",
            ".font-claude-message",
            "[data-is-streaming]",
            ".whitespace-pre-wrap",
            ".prose"
        ].join(", ");

        const elements = Array.from(
            document.querySelectorAll(selectors)
        );

        // Filter nested duplicates.
        return elements.filter((el) => {
            let parent = el.parentElement;

            while (parent) {
                if (elements.includes(parent)) {
                    return false;
                }

                parent = parent.parentElement;
            }

            return el.textContent.trim().length > 0;
        });
    }

    scanExistingMessages() {
        const messages = this.getMessages();

        messages.forEach((m, idx) => {
            const id =
                m.getAttribute("data-message-id") ||
                `msg_${idx}`;

            this.lastProcessedMessages.add(id);
        });
    }

    resyncHistory() {
        console.log(
            "Resyncing history to bridge..."
        );

        try {
            this.lastProcessedMessages.clear();

            const messages = this.getMessages();

            if (messages.length === 0) {
                if (this.conversationId !== "pending") {
                    chrome.runtime.sendMessage({
                        type: "claude_message",
                        message: {
                            type: "diagnostic",
                            content:
                                `0 messages detected on ${window.location.href} despite a conversation id in the URL. The selectors may no longer match claude.ai's markup.`
                        }
                    });
                }

                return;
            }

            messages.forEach((m, idx) => {
                this.checkForNewMessage(
                    m,
                    idx,
                    true
                );
            });
        } catch (e) {
            chrome.runtime.sendMessage({
                type: "claude_message",
                message: {
                    type: "diagnostic",
                    content:
                        "JS ERROR IN EXTENSION: " +
                        e.message +
                        "\n" +
                        e.stack
                }
            });
        }
    }

    handleMutations(mutations) {
        let textUpdated = false;
        let shouldScan = false;

        for (const mutation of mutations) {
            if (
                mutation.type === "childList" &&
                mutation.addedNodes.length > 0
            ) {
                shouldScan = true;
            }

            if (
                this.currentStreamingMessage &&
                (
                    mutation.type === "characterData" ||
                    mutation.type === "childList"
                )
            ) {
                if (
                    this.currentStreamingMessage.node.contains(
                        mutation.target
                    )
                ) {
                    textUpdated = true;
                }
            }
        }

        if (shouldScan) {
            const messages = this.getMessages();

            messages.forEach((m, idx) => {
                this.checkForNewMessage(m, idx);
            });
        }

        if (
            textUpdated &&
            this.currentStreamingMessage
        ) {
            this.sendStreamingUpdate();
        }

        if (this.currentStreamingMessage) {
            this.debounceCompletionCheck();
        }
    }

    checkForNewMessage(
        element,
        index = 0,
        historical = false
    ) {
        let msgNode = element;

        if (
            !element.matches ||
            !element.matches(
                ".font-user-message, " +
                ".font-claude-message, " +
                "[data-is-streaming], " +
                "[data-message-author-role], " +
                '[data-testid="user-message"], ' +
                '[data-testid="assistant-message"], ' +
                ".prose, " +
                ".whitespace-pre-wrap"
            )
        ) {
            if (element.querySelector) {
                const found = element.querySelector(
                    ".font-user-message, " +
                    ".font-claude-message, " +
                    "[data-is-streaming], " +
                    "[data-message-author-role], " +
                    '[data-testid="user-message"], ' +
                    '[data-testid="assistant-message"], ' +
                    ".prose"
                );

                if (found) {
                    msgNode = found;
                }
            }
        }

        if (
            msgNode &&
            msgNode.textContent &&
            msgNode.textContent.trim().length > 0
        ) {
            let isUser = false;

            const authorRole =
                msgNode.getAttribute(
                    "data-message-author-role"
                );

            if (authorRole) {
                isUser = authorRole === "user";
            } else if (
                msgNode.matches(
                    ".font-user-message, [data-testid='user-message']"
                ) ||
                (
                    msgNode.className &&
                    typeof msgNode.className === "string" &&
                    msgNode.className.includes(
                        "whitespace-pre-wrap"
                    ) &&
                    !msgNode.className.includes("prose")
                )
            ) {
                isUser = true;
            } else if (
                msgNode.matches(
                    ".font-claude-message, [data-testid='assistant-message']"
                )
            ) {
                isUser = false;
            } else {
                isUser = false;
            }

            const role = isUser
                ? "user"
                : "assistant";

            // User messages are already echoed locally by the bridge.
            if (isUser) {
                const id =
                    msgNode.getAttribute(
                        "data-message-id"
                    ) ||
                    msgNode.getAttribute(
                        "data-custom-id"
                    ) ||
                    `msg_idx_${index}`;

                if (
                    !msgNode.hasAttribute(
                        "data-message-id"
                    ) &&
                    !msgNode.hasAttribute(
                        "data-custom-id"
                    )
                ) {
                    msgNode.setAttribute(
                        "data-custom-id",
                        id
                    );
                }

                this.lastProcessedMessages.add(
                    id +
                    ":" +
                    this.extractTextContent(
                        msgNode
                    ).length
                );

                return;
            }

            const id =
                msgNode.getAttribute(
                    "data-message-id"
                ) ||
                msgNode.getAttribute(
                    "data-custom-id"
                ) ||
                `msg_idx_${index}`;

            if (
                !msgNode.hasAttribute(
                    "data-message-id"
                ) &&
                !msgNode.hasAttribute(
                    "data-custom-id"
                )
            ) {
                msgNode.setAttribute(
                    "data-custom-id",
                    id
                );
            }

            const content =
                this.extractTextContent(msgNode);

            const isStreaming =
                msgNode.getAttribute("data-is-streaming") === "true" ||
                msgNode.hasAttribute("data-is-streaming") ||
                Boolean(document.querySelector('button[aria-label="Stop generating"]'));

            const status = isStreaming ? "streaming" : "complete";

            const contentHash =
                id + ":" + content.length;

            if (
                !this.lastProcessedMessages.has(
                    contentHash
                )
            ) {
                this.lastProcessedMessages.add(
                    contentHash
                );

                if (
                    isStreaming &&
                    role === "assistant"
                ) {
                    this.currentStreamingMessage = {
                        node: msgNode,
                        id: id,
                        lastContent: content
                    };
                }

                chrome.runtime.sendMessage({
                    type: "claude_message",
                    message: {
                        type: "assistant_message",
                        conversation_id:
                            this.conversationId,
                        message_id: id,
                        role: role,
                        content: content,
                        status: status,
                        historical
                    }
                });
            }
        }
    }

    extractTextContent(node) {
        let text = "";

        const clone =
            node.cloneNode(true);

        const preElements =
            clone.querySelectorAll("pre");

        preElements.forEach((pre) => {
            const code =
                pre.querySelector("code");

            if (code) {
                const langMatch =
                    pre.className.match(
                        /language-(\w+)/
                    ) ||
                    code.className.match(
                        /language-(\w+)/
                    );

                const lang =
                    langMatch
                        ? langMatch[1]
                        : "";

                const codeText =
                    code.textContent;

                const wrapper =
                    document.createElement("div");

                wrapper.textContent =
                    `\n\`\`\`${lang}\n${codeText}\n\`\`\`\n`;

                pre.parentNode.replaceChild(
                    wrapper,
                    pre
                );
            }
        });

        const pElements =
            clone.querySelectorAll("p");

        pElements.forEach((p) => {
            p.textContent =
                p.textContent + "\n\n";
        });

        text =
            clone.textContent
                .replace(/\n{3,}/g, "\n\n")
                .trim();

        return text;
    }

    sendStreamingUpdate() {
        if (!this.currentStreamingMessage) {
            return;
        }

        const content =
            this.extractTextContent(
                this.currentStreamingMessage.node
            );

        if (
            content !==
            this.currentStreamingMessage.lastContent
        ) {
            this.currentStreamingMessage.lastContent =
                content;

            const id =
                this.currentStreamingMessage.id;

            const contentHash =
                id + ":" + content.length;

            this.lastProcessedMessages.add(
                contentHash
            );

            chrome.runtime.sendMessage({
                type: "claude_message",
                message: {
                    type: "assistant_message",
                    conversation_id:
                        this.conversationId,
                    message_id: id,
                    role: "assistant",
                    content: content,
                    status: "streaming"
                }
            });
        }
    }

    debounceCompletionCheck() {
        if (this.debounceTimer) {
            clearTimeout(
                this.debounceTimer
            );
        }

        this.debounceTimer =
            setTimeout(() => {
                const stopBtn =
                    document.querySelector(
                        'button[aria-label="Stop generating"]'
                    );

                if (!stopBtn) {
                    this.finalizeCurrentStreamingMessage();
                }
            }, 1500);
    }

    finalizeCurrentStreamingMessage() {
        if (
            !this.currentStreamingMessage
        ) {
            return;
        }

        const node =
            this.currentStreamingMessage.node;

        const id =
            this.currentStreamingMessage.id;

        const content =
            this.extractTextContent(node);

        chrome.runtime.sendMessage({
            type: "claude_message",
            message: {
                type: "assistant_message",
                conversation_id:
                    this.conversationId,
                message_id: id,
                role: "assistant",
                content: content,
                status: "complete"
            }
        });

        this.currentStreamingMessage = null;
    }

    findClaudeInput() {
        return (
            document.querySelector(
                ".ProseMirror"
            ) ||
            document.querySelector(
                'div[contenteditable="true"]'
            )
        );
    }

    /*
     * Wait until Claude has registered the attachment AND
     * its real Send button is enabled.
     *
     * We never modify disabled/aria-disabled ourselves.
     */
    async waitForAttachmentReady(
        timeoutMs = 15000
    ) {
        const started = Date.now();

        while (
            Date.now() - started <
            timeoutMs
        ) {
            const input =
                this.findClaudeInput();

            if (!input) {
                await new Promise(
                    resolve =>
                        setTimeout(
                            resolve,
                            200
                        )
                );

                continue;
            }

            const composer =
                input.closest("form") ||
                input.closest("fieldset") ||
                input.parentElement?.parentElement ||
                document;

            const fileInputs =
                Array.from(
                    document.querySelectorAll(
                        'input[type="file"]'
                    )
                );

            const hasSelectedFile =
                fileInputs.some(
                    fileInput =>
                        fileInput.files &&
                        fileInput.files.length > 0
                );

            const attachmentElement =
                document.querySelector(
                    '[data-testid*="attachment"], ' +
                    '[class*="attachment"], ' +
                    '[aria-label*="Remove"]'
                );

            const buttons =
                Array.from(
                    composer.querySelectorAll(
                        "button"
                    )
                );

            const sendBtn =
                buttons.find(button => {
                    const label = [
                        button.getAttribute(
                            "aria-label"
                        ),
                        button.getAttribute(
                            "data-testid"
                        ),
                        button.getAttribute(
                            "title"
                        ),
                        button.getAttribute(
                            "name"
                        )
                    ]
                        .filter(Boolean)
                        .join(" ")
                        .toLowerCase();

                    return (
                        label.includes("send") ||
                        label.includes("submit") ||
                        button.type === "submit"
                    );
                });

            const attachmentReady =
                hasSelectedFile ||
                !!attachmentElement;

            const sendReady =
                !!sendBtn &&
                !sendBtn.disabled &&
                sendBtn.getAttribute(
                    "aria-disabled"
                ) !== "true";

            if (
                attachmentReady &&
                sendReady
            ) {
                // Give Claude a small stabilization window.
                await new Promise(
                    resolve =>
                        setTimeout(
                            resolve,
                            400
                        )
                );

                const stillEnabled =
                    !sendBtn.disabled &&
                    sendBtn.getAttribute(
                        "aria-disabled"
                    ) !== "true";

                if (stillEnabled) {
                    return true;
                }
            }

            await new Promise(
                resolve =>
                    setTimeout(
                        resolve,
                        150
                    )
            );
        }

        this.reportDiagnostic(
            "Timed out waiting for Claude's attachment/send button to become ready."
        );

        return false;
    }

    /*
     * Insert text and submit through Claude's REAL Send button.
     *
     * There is deliberately only ONE submission mechanism:
     * sendBtn.click()
     *
     * We do NOT:
     *   - force-enable the button
     *   - call requestSubmit()
     *   - dispatch a fake Enter key
     */
    async insertAndSubmitMessage(
        text,
        waitForAttachment = false
    ) {
        const input =
            this.findClaudeInput();

        if (!input) {
            this.reportDiagnostic(
                "insertAndSubmitMessage: no Claude input field found on page"
            );

            return;
        }

        /*
         * If an attachment is being sent, wait for Claude
         * to finish processing it first.
         */
        if (waitForAttachment) {
            const attachmentReady =
                await this.waitForAttachmentReady(
                    15000
                );

            if (!attachmentReady) {
                return;
            }
        }

        input.focus();

        /*
         * Select existing composer contents.
         */
        const selection =
            window.getSelection();

        const range =
            document.createRange();

        range.selectNodeContents(input);

        selection.removeAllRanges();
        selection.addRange(range);

        /*
         * Paste the prompt into Claude's composer.
         */
        try {
            const dataTransfer =
                new DataTransfer();

            dataTransfer.setData(
                "text/plain",
                text
            );

            input.dispatchEvent(
                new ClipboardEvent(
                    "paste",
                    {
                        clipboardData:
                            dataTransfer,
                        bubbles: true,
                        cancelable: true
                    }
                )
            );
        } catch (error) {
            console.warn(
                "Claude Bridge: paste event failed",
                error
            );
        }

        /*
         * Allow React/ProseMirror to update.
         */
        await new Promise(
            resolve =>
                setTimeout(
                    resolve,
                    500
                )
        );

        /*
         * Fallback if the paste event was intercepted.
         */
        const probe =
            text
                .trim()
                .slice(
                    0,
                    Math.min(
                        15,
                        text.trim().length
                    )
                );

        if (
            !input.textContent ||
            (
                probe &&
                !input.textContent.includes(
                    probe
                )
            )
        ) {
            input.textContent = text;

            input.dispatchEvent(
                new InputEvent(
                    "input",
                    {
                        bubbles: true,
                        inputType:
                            "insertText",
                        data: text
                    }
                )
            );
        }

        /*
         * Allow Claude to update its internal state and
         * enable the real Send button.
         */
        await new Promise(
            resolve =>
                setTimeout(
                    resolve,
                    500
                )
        );

        const findSendButton = () => {
            const scope =
                input.closest("form") ||
                input.closest("fieldset") ||
                input.parentElement?.parentElement ||
                document;

            const buttons =
                Array.from(
                    scope.querySelectorAll(
                        "button"
                    )
                );

            return (
                buttons.find(button => {
                    const label = [
                        button.getAttribute(
                            "aria-label"
                        ),
                        button.getAttribute(
                            "data-testid"
                        ),
                        button.getAttribute(
                            "title"
                        ),
                        button.getAttribute(
                            "name"
                        )
                    ]
                        .filter(Boolean)
                        .join(" ")
                        .toLowerCase();

                    return (
                        label.includes(
                            "send"
                        ) ||
                        label.includes(
                            "submit"
                        ) ||
                        button.type ===
                            "submit"
                    );
                }) || null
            );
        };

        /*
         * Wait for Claude's REAL Send button.
         */
        const sendWaitStarted =
            Date.now();

        const sendTimeout =
            waitForAttachment
                ? 15000
                : 8000;

        let sendBtn = null;

        while (
            Date.now() -
                sendWaitStarted <
            sendTimeout
        ) {
            sendBtn =
                findSendButton();

            if (
                sendBtn &&
                !sendBtn.disabled &&
                sendBtn.getAttribute(
                    "aria-disabled"
                ) !== "true"
            ) {
                break;
            }

            await new Promise(
                resolve =>
                    setTimeout(
                        resolve,
                        150
                    )
            );
        }

        /*
         * Do NOT force the button enabled.
         *
         * If Claude has not enabled it, something in
         * Claude's own composer state is not ready.
         */
        if (
            !sendBtn ||
            sendBtn.disabled ||
            sendBtn.getAttribute(
                "aria-disabled"
            ) === "true"
        ) {
            this.reportDiagnostic(
                "Claude Bridge: Send button never became enabled."
            );

            return;
        }

        console.log(
            "Claude Terminal Bridge: Clicking Claude's real Send button."
        );

        /*
         * SINGLE SUBMISSION.
         *
         * No requestSubmit().
         * No fake Enter key.
         * No disabled manipulation.
         */
        sendBtn.click();
    }

    reportDiagnostic(content) {
        chrome.runtime.sendMessage({
            type: "claude_message",
            message: {
                type: "diagnostic",
                content
            }
        }).catch(() => {});
    }
}


// ---------------------------------------------------------
// Initialize adapter
// ---------------------------------------------------------

const adapter = new ClaudeAdapter();

adapter.startObserving();


// ---------------------------------------------------------
// Messages from the extension/background script
// ---------------------------------------------------------

chrome.runtime.onMessage.addListener(
    (request, sender, sendResponse) => {

        if (
            request.type ===
            "inject_message"
        ) {
            adapter.insertAndSubmitMessage(
                request.content,
                Boolean(
                    request.waitForAttachment
                )
            );

            return true;
        }

        if (
            request.type ===
            "resync_history"
        ) {
            adapter.resyncHistory();

            return true;
        }
    }
);
