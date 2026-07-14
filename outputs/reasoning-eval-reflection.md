# Reflection experiment (teach → apply)

Model `hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS`, think tier, reflect_gate=Always.

Teach task `R_teach`: ok=true, reflected=true, lessons stored=0.

| apply run | store | ok | wall ms | tools | graph attempts | routed-around |
|---|---|---|---|---|---|---|
| R_apply | with lesson | true | 51708 | 4 | 0 | false |
| R_apply | clean | true | 47694 | 3 | 0 | false |

Hypothesis: with the teach-lesson present, the apply PLAN should avoid the dead knowledge-graph tool (fewer graph attempts) and/or reach the answer faster. Read the numbers, not the hope.
