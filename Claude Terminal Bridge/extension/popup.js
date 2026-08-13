document.addEventListener('DOMContentLoaded', () => {
    const tokenInput = document.getElementById('token-input');
    const saveTokenBtn = document.getElementById('save-token-btn');
    const rustStatus = document.getElementById('rust-status');
    const claudeStatus = document.getElementById('claude-status');
    const convList = document.getElementById('conversations-list');

    // Load saved token
    chrome.storage.local.get(['sessionToken'], (res) => {
        if (res.sessionToken) {
            tokenInput.value = res.sessionToken;
        }
    });

    saveTokenBtn.addEventListener('click', () => {
        const token = tokenInput.value.trim();
        if (token) {
            chrome.storage.local.set({ sessionToken: token }, () => {
                saveTokenBtn.textContent = "Saved!";
                setTimeout(() => { saveTokenBtn.textContent = "Connect"; }, 2000);
            });
        }
    });

    function updateStatus() {
        chrome.runtime.sendMessage({ type: "get_status" }, (response) => {
            if (!response) return;

            // Rust status
            if (response.bridgeConnected) {
                rustStatus.textContent = "Connected";
                rustStatus.className = "badge connected";
            } else {
                rustStatus.textContent = "Disconnected";
                rustStatus.className = "badge disconnected";
            }

            // Claude status
            if (response.claudeTabs && response.claudeTabs.length > 0) {
                claudeStatus.textContent = "Connected";
                claudeStatus.className = "badge connected";
                
                // Update list
                convList.innerHTML = '';
                response.claudeTabs.forEach(tab => {
                    const div = document.createElement('div');
                    div.className = 'tab-item';
                    
                    // Simple heuristic to get conversation ID from URL
                    const parts = tab.url.split('/');
                    const id = parts[parts.length - 1];
                    div.textContent = `ID: ${id}`;
                    
                    convList.appendChild(div);
                });
            } else {
                claudeStatus.textContent = "Disconnected";
                claudeStatus.className = "badge disconnected";
                convList.innerHTML = '<p class="empty-state">No active Claude tabs detected.</p>';
            }
        });
    }

    // Initial update
    updateStatus();
    
    // Poll for updates every second while popup is open
    setInterval(updateStatus, 1000);
});
