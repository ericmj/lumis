defmodule Lumis.PortTest do
  use ExUnit.Case, async: true

  alias Lumis.Formatter.HTML

  @source "defmodule App do\n  def run(value), do: IO.puts(\"<\#{value}>\")\nend\n"
  @linked_attrs Map.new(HTML.classes(), fn {scope, class} -> {scope, ~s|class="#{class}"|} end)

  setup do
    port = Lumis.Port.open(preload_installed: true)
    assert {:ok, _max_rss_kb} = Lumis.Port.await_ready(port, 30_000)
    on_exit(fn -> close(port) end)
    %{port: port}
  end

  test "a document is the NIF's html-linked HTML", %{port: port} do
    assert {:ok, html, max_rss_kb} = highlight(port, :document, @source, "lib/app.ex")
    assert html == Lumis.highlight!(@source, formatter: {:html_linked, language: "lib/app.ex"})
    assert max_rss_kb > 0
  end

  test "lines are the NIF's line fragments", %{port: port} do
    source = @source <> "\n\n"
    {:ok, events} = Lumis.highlight_events(source, "lib/app.ex")

    assert {:ok, lines, _max_rss_kb} = highlight(port, :lines, source, "lib/app.ex")
    assert lines == HTML.render_lines_from_events(source, events, @linked_attrs)
    assert length(lines) == 5
  end

  test "a language whose parser is not installed is plain text", %{port: port} do
    source = "signal clk : std_logic;"

    assert {:ok, html, _max_rss_kb} = highlight(port, :document, source, "vhdl")
    assert html == Lumis.highlight!(source, formatter: {:html_linked, language: "vhdl"})
    refute html =~ ~r/class="l-(?!line)/
  end

  test "the time limit returns the source as plain text", %{port: port} do
    source = String.duplicate(@source, 2_000)

    assert {:ok, html, _max_rss_kb} =
             highlight(port, :document, source, "lib/app.ex", time_limit: 1)

    assert html =~ ~s(data-lumis-budget="time")
    refute html =~ "l-keyword"
  end

  test "answers each request on the same process", %{port: port} do
    assert {:ok, elixir, _} = highlight(port, :document, ":ok", "lib/app.ex")
    assert {:ok, json, _} = highlight(port, :document, "[1]", "data.json")
    assert elixir =~ "language-elixir"
    assert json =~ "language-json"
  end

  test "reports an error for source that is not UTF-8", %{port: port} do
    assert {:error, "source is not UTF-8", _} =
             highlight(port, :document, <<0xFF>>, "lib/app.ex")
  end

  test "the process exits when the port closes", %{port: port} do
    {:os_pid, os_pid} = Port.info(port, :os_pid)
    assert alive?(os_pid)

    Port.close(port)

    assert Enum.any?(1..100, fn _ ->
             Process.sleep(10)
             not alive?(os_pid)
           end)
  end

  test "a CPU limit ends a request with SIGXCPU", %{port: port} do
    source = String.duplicate(@source, 100_000)

    Port.command(port, Lumis.Port.request(:document, source, "lib/app.ex", cpu_limit_ms: 1))

    # 128 + SIGXCPU, which is 24 on both Linux and macOS.
    assert_receive {^port, {:exit_status, 152}}, 10_000
  end

  test "decoding a reply that is not one returns an error" do
    assert Lumis.Port.decode_reply(<<9>>, :document) == {:error, :malformed_reply}

    assert Lumis.Port.decode_reply(<<0, 1::32, 5::32, "ab">>, :lines) ==
             {:error, :malformed_reply}
  end

  defp highlight(port, kind, source, language, opts \\ []) do
    Port.command(port, Lumis.Port.request(kind, source, language, opts))

    receive do
      {^port, {:data, reply}} -> Lumis.Port.decode_reply(reply, kind)
    after
      30_000 -> flunk("no reply")
    end
  end

  defp close(port) do
    Port.close(port)
  rescue
    ArgumentError -> :ok
  end

  defp alive?(os_pid) do
    {_, status} = System.cmd("kill", ["-0", to_string(os_pid)], stderr_to_stdout: true)
    status == 0
  end
end
