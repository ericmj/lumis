defmodule Lumis.PortTest do
  use ExUnit.Case, async: true

  @source "defmodule App do\n  def run(value), do: IO.puts(\"<\#{value}>\")\nend\n"

  test "highlights the same as the NIF" do
    port = Lumis.Port.open(preload: ["elixir"])

    assert {:ok, html, [], max_rss_kb} = highlight(port, @source, "lib/app.ex")
    assert html == Lumis.highlight!(@source, formatter: {:html_linked, language: "lib/app.ex"})
    assert max_rss_kb > 0

    Port.close(port)
  end

  test "answers each request on the same process" do
    port = Lumis.Port.open()

    assert {:ok, elixir, [], _} = highlight(port, ":ok", "lib/app.ex")
    assert {:ok, json, [], _} = highlight(port, "[1]", "data.json")
    assert elixir =~ "language-elixir"
    assert json =~ "language-json"

    Port.close(port)
  end

  test "reports languages that are not cached" do
    port = Lumis.Port.open()

    assert {:not_cached, "erlang", _} = highlight(port, "-module(app).", "src/app.erl")

    assert {:ok, html, ["haskell"], _} =
             highlight(port, "```haskell\nmain = pure ()\n```\n", "README.md")

    assert html =~ "main = pure ()"

    Port.close(port)
  end

  test "reports an error for source that is not UTF-8" do
    port = Lumis.Port.open()

    assert {:error, "source is not UTF-8", _} = highlight(port, <<0xFF>>, "lib/app.ex")

    Port.close(port)
  end

  test "the process exits when the port closes" do
    port = Lumis.Port.open()
    {:os_pid, os_pid} = Port.info(port, :os_pid)
    assert alive?(os_pid)

    Port.close(port)

    assert Enum.any?(1..100, fn _ ->
             Process.sleep(10)
             not alive?(os_pid)
           end)
  end

  test "a CPU limit ends a request with SIGXCPU" do
    port = Lumis.Port.open(preload: ["elixir"])
    source = String.duplicate(@source, 100_000)

    Port.command(port, Lumis.Port.request(source, "lib/app.ex", cpu_limit_ms: 1))

    # 128 + SIGXCPU, which is 24 on both Linux and macOS.
    assert_receive {^port, {:exit_status, 152}}, 10_000
  end

  test "cache reports a language it cannot cache" do
    assert {:error, output} = Lumis.Port.cache(["not-a-language"])
    assert output =~ "not-a-language"
  end

  defp highlight(port, source, language) do
    Port.command(port, Lumis.Port.request(source, language))

    receive do
      {^port, {:data, reply}} -> Lumis.Port.decode_reply(reply)
    after
      10_000 -> flunk("no reply")
    end
  end

  defp alive?(os_pid) do
    {_, status} = System.cmd("kill", ["-0", to_string(os_pid)], stderr_to_stdout: true)
    status == 0
  end
end
