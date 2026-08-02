import { TextInput } from "../../components/ui/TextInput.tsx";
import { SettingsField } from "./SettingsField.tsx";
import type { TasciPricingDraft } from "./tasciPricing.ts";

/** Edits the exact token-rate representation stored with a configured Tasci model. */
export function TasciPricingEditor({
  draft,
  onChange,
}: {
  draft: TasciPricingDraft;
  onChange: (draft: TasciPricingDraft) => void;
}) {
  return (
    <div className="mt-3 grid gap-3 sm:grid-cols-2">
      <p className="text-[10px] leading-4 text-subtle sm:col-span-2">
        Prices use the currency’s minor unit and apply to the specified number of tokens. The version identifies the rate set copied into usage history.
      </p>
      <SettingsField label="Pricing version">
        <TextInput
          className="w-full font-mono"
          required
          value={draft.catalogVersion}
          onChange={(event) => onChange({ ...draft, catalogVersion: event.target.value })}
        />
      </SettingsField>
      <SettingsField label="Priced token count">
        <TextInput
          className="w-full"
          inputMode="numeric"
          min="1"
          required
          type="number"
          value={draft.tokenCount}
          onChange={(event) => onChange({ ...draft, tokenCount: event.target.value })}
        />
      </SettingsField>
      <SettingsField label="Currency">
        <TextInput
          className="w-full font-mono uppercase"
          maxLength={3}
          placeholder="USD"
          required
          value={draft.currency}
          onChange={(event) => onChange({ ...draft, currency: event.target.value.toUpperCase() })}
        />
      </SettingsField>
      <SettingsField label="Input price">
        <PricingAmountInput
          required
          value={draft.inputAmount}
          onChange={(inputAmount) => onChange({ ...draft, inputAmount })}
        />
      </SettingsField>
      <SettingsField label="Output price">
        <PricingAmountInput
          required
          value={draft.outputAmount}
          onChange={(outputAmount) => onChange({ ...draft, outputAmount })}
        />
      </SettingsField>
      <SettingsField label="Cache-read input price">
        <PricingAmountInput
          value={draft.cacheReadInputAmount}
          onChange={(cacheReadInputAmount) => onChange({ ...draft, cacheReadInputAmount })}
        />
      </SettingsField>
      <SettingsField label="Cache-write input price">
        <PricingAmountInput
          value={draft.cacheWriteInputAmount}
          onChange={(cacheWriteInputAmount) => onChange({ ...draft, cacheWriteInputAmount })}
        />
      </SettingsField>
      <p className="text-[10px] leading-4 text-subtle sm:col-span-2">
        For example, USD 2.50 is 250. Omitted cache rates use the ordinary input price. Change the pricing version whenever the rates change.
      </p>
    </div>
  );
}

function PricingAmountInput({
  value,
  required = false,
  onChange,
}: {
  value: string;
  required?: boolean;
  onChange: (value: string) => void;
}) {
  return (
    <TextInput
      className="w-full"
      inputMode="numeric"
      min="0"
      placeholder={required ? undefined : "Optional"}
      required={required}
      type="number"
      value={value}
      onChange={(event) => onChange(event.target.value)}
    />
  );
}
