import { createFormHook } from "@tanstack/react-form";

import { FormFields, FormRoot, SubmissionError, SubmitButton } from "./FormComponents.tsx";
import { fieldContext, formContext } from "./FormContext.ts";
import { PodChooserField, PortField, TextField } from "./FormFields.tsx";

export const { useAppForm, withForm } = createFormHook({
  fieldComponents: {
    PodChooserField,
    PortField,
    TextField,
  },
  formComponents: {
    FormFields,
    FormRoot,
    SubmissionError,
    SubmitButton,
  },
  fieldContext,
  formContext,
});

export { focusFirstInvalidField } from "./FormComponents.tsx";
