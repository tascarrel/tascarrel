export const FULL_HEIGHT_DIFFS_CSS = `
  :host {
    display: flex;
    height: 100%;
    min-height: 0;
    flex-direction: column;
  }

  [data-diff],
  [data-file] {
    display: flex;
    min-height: 0;
    flex: 1 1 auto;
    flex-direction: column;
  }

  [data-code] {
    min-height: 0;
    flex: 1 1 auto;
    align-self: stretch;
    align-content: start;
    overflow: auto;
  }
`;
