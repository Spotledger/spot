using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner;

/// One row of the problem list: what a person reads and the problem it stands for.
public sealed record FlowProblemRow(string Text, FlowProblem Problem, string AutomationName)
{

    public bool HasTarget => Problem.NodeId is not null || Problem.Edge is not null;
}

/// The problem list: the server's problems and the local hints as rows, errors first, each row
/// naming its code so a reviewer can find the rule and its node so a click can focus it. The list
/// belongs to the host's panel; this type only decides the order and the text, so the panel needs no
/// logic of its own.
public static class FlowProblemList
{

    public static IReadOnlyList<FlowProblemRow> Rows(IReadOnlyList<FlowProblem>? problems)
    {
        if (problems is null || problems.Count == 0)
        {
            return [];
        }

        List<FlowProblemRow> rows = [];
        foreach (FlowProblem problem in problems
            .OrderBy(problem => problem.Severity)
            .ThenBy(problem => problem.NodeId ?? problem.EdgeFrom ?? string.Empty, StringComparer.Ordinal))
        {
            string where = problem.NodeId ?? problem.EdgeFrom ?? string.Empty;
            string text = problem.Code + ": " + problem.Message;
            rows.Add(new FlowProblemRow(
                text,
                problem,
                string.IsNullOrEmpty(where) ? text : text + " (" + where + ")"));
        }

        return rows;
    }
}
