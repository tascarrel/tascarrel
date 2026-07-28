import type { LanguageRegistration } from "shiki/core";

// This mirrors sidex-syntax's hand-written lexer and contextual parser instead of an editor grammar.
const SIDEX_IDENTIFIER = "[_\\p{Alphabetic}][_\\p{Alphabetic}\\p{Number}]*";
const SIDEX_ITEM_PREFIX = "(?:^\\s*|(?<=\\])\\s+)";

export const SIDEX_LANGUAGE: LanguageRegistration = {
  name: "sidex",
  displayName: "Sidex",
  aliases: ["Sidex"],
  scopeName: "source.sidex",
  patterns: [
    { include: "#comments" },
    { include: "#attributes" },
    { include: "#imports" },
    { include: "#recordDefinitions" },
    { include: "#variantDefinitions" },
    { include: "#inlineDefinitions" },
    { include: "#literals" },
    { include: "#punctuation" },
  ],
  repository: {
    comments: {
      patterns: [
        {
          match: "//!.*$",
          name: "comment.line.documentation.inline.sidex",
        },
        {
          match: "///.*$",
          name: "comment.line.documentation.preceding.sidex",
        },
        {
          match: "//.*$",
          name: "comment.line.double-slash.sidex",
        },
        {
          begin: "/\\*",
          beginCaptures: {
            0: { name: "punctuation.definition.comment.begin.sidex" },
          },
          end: "\\*/",
          endCaptures: {
            0: { name: "punctuation.definition.comment.end.sidex" },
          },
          name: "comment.block.sidex",
        },
      ],
    },
    literals: {
      patterns: [
        {
          begin: "\"",
          beginCaptures: {
            0: { name: "punctuation.definition.string.begin.sidex" },
          },
          end: "\"|(?=$)",
          endCaptures: {
            0: { name: "punctuation.definition.string.end.sidex" },
          },
          name: "string.quoted.double.sidex",
          patterns: [
            {
              match: "\\\\[\\\\\"]",
              name: "constant.character.escape.sidex",
            },
            {
              match: "\\\\.",
              name: "invalid.illegal.escape.sidex",
            },
          ],
        },
        {
          match: "(?<![_\\p{Alphabetic}\\p{Number}])-?[0-9]+(?:\\.[0-9]+)?",
          name: "constant.numeric.sidex",
        },
        {
          match: `(?<![_\\p{Alphabetic}\\p{Number}])(?:true|false)(?![_\\p{Alphabetic}\\p{Number}])`,
          name: "constant.language.boolean.sidex",
        },
      ],
    },
    punctuation: {
      patterns: [
        {
          match: "::|[?:=]",
          name: "keyword.operator.sidex",
        },
        {
          match: "[()\\[\\]{}<>,;]",
          name: "punctuation.sidex",
        },
        {
          match: "[+\\-%/.*#$^&!]",
          name: "punctuation.sidex",
        },
      ],
    },
    attributeBody: {
      patterns: [
        { include: "#comments" },
        { include: "#literals" },
        {
          begin: "\\(",
          end: "\\)",
          patterns: [{ include: "#attributeBody" }],
        },
        {
          begin: "\\[",
          end: "\\]",
          patterns: [{ include: "#attributeBody" }],
        },
        {
          begin: "\\{",
          end: "\\}",
          patterns: [{ include: "#attributeBody" }],
        },
        {
          match: SIDEX_IDENTIFIER,
          name: "variable.other.attribute.sidex",
        },
        { include: "#punctuation" },
      ],
    },
    attributes: {
      patterns: [
        {
          begin: "(#)(\\[)",
          beginCaptures: {
            1: { name: "punctuation.definition.annotation.sidex" },
            2: { name: "punctuation.section.brackets.begin.sidex" },
          },
          end: "\\]",
          endCaptures: {
            0: { name: "punctuation.section.brackets.end.sidex" },
          },
          name: "meta.annotation.sidex",
          patterns: [{ include: "#attributeBody" }],
        },
      ],
    },
    typeExpression: {
      patterns: [
        { include: "#comments" },
        { include: "#attributes" },
        { include: "#literals" },
        {
          match: SIDEX_IDENTIFIER,
          name: "support.type.sidex",
        },
        { include: "#punctuation" },
      ],
    },
    imports: {
      patterns: [
        {
          begin: `${SIDEX_ITEM_PREFIX}(import)\\b`,
          beginCaptures: {
            1: { name: "keyword.control.import.sidex" },
          },
          end: "$",
          name: "meta.import.sidex",
          patterns: [
            { include: "#comments" },
            {
              match: SIDEX_IDENTIFIER,
              name: "entity.name.namespace.sidex",
            },
            { include: "#punctuation" },
          ],
        },
      ],
    },
    recordDefinitions: {
      patterns: [
        {
          begin: `${SIDEX_ITEM_PREFIX}(record)\\s+(${SIDEX_IDENTIFIER})`,
          beginCaptures: {
            1: { name: "keyword.declaration.record.sidex" },
            2: { name: "entity.name.type.record.sidex" },
          },
          end: "\\}",
          endCaptures: {
            0: { name: "punctuation.section.block.end.sidex" },
          },
          name: "meta.definition.record.sidex",
          patterns: [
            { include: "#comments" },
            { include: "#attributes" },
            {
              captures: {
                1: { name: "punctuation.separator.sidex" },
                2: { name: "variable.other.field.sidex" },
                3: { name: "keyword.operator.optional.sidex" },
                4: { name: "keyword.operator.type.sidex" },
              },
              match: `(?:^\\s*|([,{])\\s*)(${SIDEX_IDENTIFIER})(\\?)?\\s*(:)`,
            },
            { include: "#typeExpression" },
          ],
        },
      ],
    },
    variantDefinitions: {
      patterns: [
        {
          begin: `${SIDEX_ITEM_PREFIX}(variant)\\s+(${SIDEX_IDENTIFIER})`,
          beginCaptures: {
            1: { name: "keyword.declaration.variant.sidex" },
            2: { name: "entity.name.type.variant.sidex" },
          },
          end: "\\}",
          endCaptures: {
            0: { name: "punctuation.section.block.end.sidex" },
          },
          name: "meta.definition.variant.sidex",
          patterns: [
            { include: "#comments" },
            { include: "#attributes" },
            {
              captures: {
                1: { name: "punctuation.separator.sidex" },
                2: { name: "entity.name.enum.member.sidex" },
              },
              match: `(?:^\\s*|([,{])\\s*)(${SIDEX_IDENTIFIER})(?=\\s*(?::|,|\\}))`,
            },
            { include: "#typeExpression" },
          ],
        },
      ],
    },
    inlineDefinitions: {
      patterns: [
        {
          begin: `${SIDEX_ITEM_PREFIX}(alias|wrapper)\\s+(${SIDEX_IDENTIFIER})\\s*(:)`,
          beginCaptures: {
            1: { name: "keyword.declaration.type.sidex" },
            2: { name: "entity.name.type.sidex" },
            3: { name: "keyword.operator.type.sidex" },
          },
          end: "$",
          name: "meta.definition.type.sidex",
          patterns: [{ include: "#typeExpression" }],
        },
        {
          begin: `${SIDEX_ITEM_PREFIX}(opaque)\\s+(${SIDEX_IDENTIFIER})`,
          beginCaptures: {
            1: { name: "keyword.declaration.opaque.sidex" },
            2: { name: "entity.name.type.opaque.sidex" },
          },
          end: "$",
          name: "meta.definition.opaque.sidex",
          patterns: [{ include: "#typeExpression" }],
        },
      ],
    },
  },
};
