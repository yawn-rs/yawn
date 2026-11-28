(comment) @comment

["component" "pub" "view" "style" "if" "else" "for" "key" "where"] @keyword

(component name: (component_name) @type)
(param name: (param_name) @variable.parameter)
(param "mut" @keyword)
(param type: (rust_type) @type)
(type_parameters (rust_type) @type)
(where_clause (rust_type) @type)
(component_tag) @type
(tag_name) @tag
(attribute_name) @attribute
(directive) @keyword
(attribute "=" @operator)

(string) @string
(string_content) @string
(escape) @string.escape
(interpolation ["{" "}"] @punctuation.special)

(expression ["{" "}"] @punctuation.special)
["<" ">" "</" "/>"] @punctuation.bracket
["(" ")" "{" "}"] @punctuation.bracket
