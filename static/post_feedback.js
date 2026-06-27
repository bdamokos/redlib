(() => {
    const nativeSubmit = HTMLFormElement.prototype.submit;
    const channelName = "redlib-post-feedback";
    const channel = "BroadcastChannel" in window ? new BroadcastChannel(channelName) : null;

    const feedbackGroups = (root = document) => {
        const container = root || document;
        if (container instanceof Element && container.matches("[data-post-feedback]")) {
            return [container];
        }
        return Array.from(container.querySelectorAll?.("[data-post-feedback]") || []);
    };

    const groupKey = (group) => `${group.dataset.postId || ""}:${group.dataset.feedbackModel || "manual"}`;

    const matchingGroups = (group) => {
        const key = groupKey(group);
        return feedbackGroups(document).filter((candidate) => groupKey(candidate) === key);
    };

    const setPending = (group, pending) => {
        for (const candidate of matchingGroups(group)) {
            candidate.dataset.feedbackPending = pending ? "true" : "false";
            for (const button of candidate.querySelectorAll("button")) {
                button.disabled = pending;
            }
        }
    };

    const setGroupSignal = (group, signal) => {
        const nextSignal = signal === "like" || signal === "dislike" ? signal : "";
        group.dataset.feedbackCurrentSignal = nextSignal;

        for (const form of group.querySelectorAll("[data-feedback-signal]")) {
            const formSignal = form.dataset.feedbackSignal || "";
            const button = form.querySelector("button");
            if (formSignal === "clear") {
                form.hidden = nextSignal === "";
            }
            if (!button) {
                continue;
            }
            const isActive = formSignal === nextSignal && nextSignal !== "";
            button.classList.toggle("active", isActive);
            button.setAttribute("aria-pressed", isActive ? "true" : "false");
        }
    };

    const syncSignal = (sourceGroup, signal) => {
        for (const group of matchingGroups(sourceGroup)) {
            setGroupSignal(group, signal);
        }
    };

    const emitFeedbackSaved = (group, signal, requestedSignal, source) => {
        group.dispatchEvent(new CustomEvent("redlib:post-feedback", {
            bubbles: true,
            detail: {
                postId: group.dataset.postId || "",
                model: group.dataset.feedbackModel || "manual",
                signal,
                requestedSignal,
                source,
            },
        }));
    };

    const broadcastSignal = (group, signal) => {
        const message = {
            type: "post-feedback",
            postId: group.dataset.postId || "",
            model: group.dataset.feedbackModel || "manual",
            signal,
            time: Date.now(),
        };
        channel?.postMessage(message);
        try {
            localStorage.setItem(`${channelName}-updated`, JSON.stringify(message));
        } catch (_error) {
            // BroadcastChannel is the primary path; storage is just a fallback.
        }
    };

    const applyMessage = (message) => {
        if (message?.type !== "post-feedback") {
            return;
        }
        for (const group of feedbackGroups(document)) {
            if ((group.dataset.postId || "") === message.postId && (group.dataset.feedbackModel || "manual") === message.model) {
                setGroupSignal(group, message.signal || "");
            }
        }
    };

    const bindForm = (form) => {
        if (form.dataset.postFeedbackBound === "true") {
            return;
        }
        form.dataset.postFeedbackBound = "true";

        form.addEventListener("submit", async (event) => {
            event.preventDefault();

            const group = form.closest("[data-post-feedback]");
            if (!group || group.dataset.feedbackPending === "true") {
                return;
            }

            const requestedSignal = form.dataset.feedbackSignal || "";
            const source = form.dataset.postFeedbackSource || "";
            delete form.dataset.postFeedbackSource;

            setPending(group, true);
            try {
                const response = await fetch(form.action, {
                    method: "POST",
                    body: new URLSearchParams(new FormData(form)).toString(),
                    credentials: "same-origin",
                    headers: {
                        "X-Redlib-Ajax": "1",
                        "Content-Type": "application/x-www-form-urlencoded; charset=UTF-8",
                    },
                });
                if (!response.ok) {
                    throw new Error(`Feedback request failed: ${response.status}`);
                }
                const payload = await response.json();
                const nextSignal = payload?.signal || "";
                syncSignal(group, nextSignal);
                broadcastSignal(group, nextSignal);
                emitFeedbackSaved(group, nextSignal, requestedSignal, source);
            } catch (_error) {
                setPending(group, false);
                nativeSubmit.call(form);
                return;
            }

            setPending(group, false);
        });
    };

    const initializePostFeedback = (root = document) => {
        for (const group of feedbackGroups(root)) {
            setGroupSignal(group, group.dataset.feedbackCurrentSignal || "");
            for (const form of group.querySelectorAll("[data-post-feedback-form]")) {
                bindForm(form);
            }
        }
    };

    const submitPostFeedback = (post, signal, options = {}) => {
        if (!(post instanceof Element)) {
            return false;
        }
        const group = post.querySelector("[data-post-feedback]");
        if (!group || group.dataset.feedbackPending === "true") {
            return false;
        }
        const form = group.querySelector(`[data-feedback-signal='${signal}']`);
        if (!(form instanceof HTMLFormElement)) {
            return false;
        }
        if (options.source) {
            form.dataset.postFeedbackSource = options.source;
        }
        if (typeof form.requestSubmit === "function") {
            form.requestSubmit();
        } else {
            form.querySelector("button")?.click();
        }
        return true;
    };

    window.initializePostFeedback = initializePostFeedback;
    window.submitPostFeedback = submitPostFeedback;

    channel?.addEventListener("message", (event) => applyMessage(event.data));
    window.addEventListener("storage", (event) => {
        if (event.key !== `${channelName}-updated` || !event.newValue) {
            return;
        }
        try {
            applyMessage(JSON.parse(event.newValue));
        } catch (_error) {
            // Ignore malformed storage events from older tabs.
        }
    });
    document.addEventListener("DOMContentLoaded", () => initializePostFeedback(document));
})();
