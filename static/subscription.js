document.addEventListener("DOMContentLoaded", () => {
    const forms = Array.from(document.querySelectorAll("form[data-subscription-form]"));
    if (forms.length === 0) {
        return;
    }

    const nativeSubmit = HTMLFormElement.prototype.submit;

    const updateFormState = (form, nextState) => {
        const button = form.querySelector("button");
        if (!button) {
            return;
        }

        const isSubscribed = nextState === "subscribed";
        const buttonClass = isSubscribed ? form.dataset.unsubscribeClass : form.dataset.subscribeClass;
        const buttonLabel = isSubscribed ? form.dataset.unsubscribeLabel : form.dataset.subscribeLabel;
        const action = isSubscribed ? form.dataset.unsubscribeAction : form.dataset.subscribeAction;

        form.dataset.subscriptionState = nextState;

        if (action) {
            form.action = action;
        }

        if (buttonClass) {
            button.className = buttonClass;
        }

        if (buttonLabel) {
            button.textContent = buttonLabel;
        }
    };

    const setPending = (target, pending) => {
        for (const form of forms) {
            if (form.dataset.subscriptionTarget !== target) {
                continue;
            }

            form.dataset.subscriptionPending = pending ? "true" : "false";

            const button = form.querySelector("button");
            if (button) {
                button.disabled = pending;
            }
        }
    };

    const toggleTarget = (target) => {
        for (const form of forms) {
            if (form.dataset.subscriptionTarget !== target) {
                continue;
            }

            const nextState = form.dataset.subscriptionState === "subscribed" ? "unsubscribed" : "subscribed";
            updateFormState(form, nextState);
        }
    };

    for (const form of forms) {
        form.addEventListener("submit", async (event) => {
            event.preventDefault();

            const target = form.dataset.subscriptionTarget || "";
            if (target === "" || form.dataset.subscriptionPending === "true") {
                return;
            }

            setPending(target, true);

            try {
                const response = await fetch(form.action, {
                    method: "POST",
                    credentials: "same-origin",
                    headers: {
                        "X-Redlib-Ajax": "1",
                    },
                });

                if (!response.ok) {
                    throw new Error(`Subscription request failed: ${response.status}`);
                }

                toggleTarget(target);
            } catch (_error) {
                setPending(target, false);
                nativeSubmit.call(form);
                return;
            }

            setPending(target, false);
        });
    }
});
