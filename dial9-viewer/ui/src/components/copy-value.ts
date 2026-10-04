import { copyText } from "../lib/url/copy-link.js";

const feedbackTimers = new WeakMap<HTMLButtonElement, ReturnType<typeof setTimeout>>();

/** Copy a full field value, with the viewer's brief button feedback. */
export const copyValue = async (event: MouseEvent, value: string): Promise<void> => {
  const button = event.currentTarget as HTMLButtonElement;
  clearTimeout(feedbackTimers.get(button));
  button.disabled = true;
  try {
    await copyText(value);
    button.textContent = "✓";
    button.title = "Copied";
  } catch (error) {
    console.warn("copy-value: clipboard write failed:", error);
    button.textContent = "!";
    button.title = "Copy failed";
  } finally {
    button.disabled = false;
    // Revert the feedback after 800ms - imperative, no store.
    feedbackTimers.set(button, setTimeout(() => {
      button.textContent = "⎘";
      button.title = "Copy value";
      feedbackTimers.delete(button);
    }, 800));
  }
};
