defmodule Lumis.Port do
  @moduledoc """
  Highlights in a separate `lumis serve` OS process, over an Erlang port.

  The NIF highlights inside the VM, where a highlight cannot be stopped once it
  has started and a crash in the parser takes the VM with it. `lumis serve` runs
  the same highlighting in its own process: closing the port stops it, also in
  the middle of a highlight, and a crash only ends that process.

      port = Lumis.Port.open(preload_installed: true)
      {:ok, _max_rss_kb} = Lumis.Port.await_ready(port, 10_000)

      Port.command(port, Lumis.Port.request(:document, "IO.puts(:ok)", "lib/app.ex"))

      receive do
        {^port, {:data, reply}} -> Lumis.Port.decode_reply(reply, :document)
      end
      #=> {:ok, "<pre class=\\"lumis\\">...", 31_412}

  A `:document` is html-linked HTML, the same as
  `Lumis.highlight!(source, formatter: {:html_linked, language: language})`.
  `:lines` is one html-linked fragment per line, the same as
  `Lumis.Formatter.HTML.render_lines_from_events/3` with each scope's class.

  Parsers come from the same directories the NIF reads, the `priv/parsers` of
  every `lumis_wasm_*` dependency. A language without one renders as plain text.
  The process never downloads.
  """

  @document 1
  @lines 2

  @ok 0
  @error 1
  @ready 2

  @typedoc "Peak resident set size of the `lumis serve` process, in kilobytes."
  @type max_rss_kb() :: non_neg_integer()

  @type kind() :: :document | :lines

  @type reply() ::
          {:ok, html :: String.t() | [String.t()], max_rss_kb()}
          | {:error, message :: String.t(), max_rss_kb()}
          | {:ready, max_rss_kb()}
          | {:error, :malformed_reply}

  @doc """
  Path to the `lumis` executable this package builds.
  """
  @spec executable() :: Path.t()
  def executable do
    Application.app_dir(:lumis, "priv/native/lumis")
  end

  @doc """
  Starts `lumis serve` and returns its port, owned by the calling process.

  The process writes a ready reply once it has loaded what it was asked to
  preload, see `await_ready/2`. The port then sends `{port, {:data, reply}}` for
  every request, and `{port, {:exit_status, status}}` when the process exits.

  ## Options

    * `:preload_installed` - load every installed language before the ready
      reply, so no request pays for loading one. Defaults to `false`.
    * `:max_data_mb` - heap limit of the process, applied with `RLIMIT_DATA`.
      An allocation past it fails in that process. Linux only.
    * `:parser_dirs` - defaults to the parsers this project depends on.
    * `:data_dir` - where compiled parser modules are kept, defaults to the
      directory the NIF uses.
    * `:wrapper` - a command and arguments to run `lumis` under, such as
      `["nice", "-n", "10", "--"]`. It has to exec `lumis` rather than fork it,
      so the port's OS process is `lumis` itself and closing the port reaches it.

  """
  @spec open(keyword()) :: port()
  def open(opts \\ []) do
    data_dir = Keyword.get_lazy(opts, :data_dir, &Lumis.Application.resolved_data_dir/0)
    parser_dirs = Keyword.get_lazy(opts, :parser_dirs, &Lumis.Packages.installed_dirs/0)

    [program | args] =
      Keyword.get(opts, :wrapper, []) ++
        [executable()] ++
        if(data_dir, do: ["--data-dir", data_dir], else: []) ++
        ["serve"] ++
        Enum.flat_map(parser_dirs, &["--parser-dir", &1]) ++
        if(Keyword.get(opts, :preload_installed, false), do: ["--preload-installed"], else: []) ++
        if(max_data_mb = opts[:max_data_mb],
          do: ["--max-data-mb", to_string(max_data_mb)],
          else: []
        )

    Port.open({:spawn_executable, program}, [
      :binary,
      :exit_status,
      :use_stdio,
      :hide,
      packet: 4,
      args: args
    ])
  end

  @doc """
  Waits for the ready reply of a port from `open/1`.
  """
  @spec await_ready(port(), timeout()) ::
          {:ok, max_rss_kb()} | {:error, {:exit_status, integer()} | :timeout | :malformed_reply}
  def await_ready(port, timeout) do
    receive do
      {^port, {:data, reply}} ->
        case decode_reply(reply, :document) do
          {:ready, max_rss_kb} -> {:ok, max_rss_kb}
          _other -> {:error, :malformed_reply}
        end

      {^port, {:exit_status, status}} ->
        {:error, {:exit_status, status}}
    after
      timeout -> {:error, :timeout}
    end
  end

  @doc """
  Encodes a request to highlight `source` as `language`, a language name or a
  file path, into a `:document` or into `:lines`.

  ## Options

    * `:match_limit` - the query match limit, as `Lumis.highlight/2` takes it in
      `:budget`. Defaults to the Tree-sitter default.
    * `:time_limit` - milliseconds the highlight may take before the source
      comes back as plain text, as `Lumis.highlight/2` takes it in `:budget`. It
      relies on Tree-sitter stopping when asked, which a query does not always
      do, so `:cpu_limit_ms` is the bound that holds. Defaults to no limit.
    * `:cpu_limit_ms` - CPU time the request may use. Past it, the kernel kills
      the process with `SIGXCPU`. It is enforced in whole seconds, so it rounds
      up. Defaults to no limit.

  """
  @spec request(kind(), String.t(), String.t(), keyword()) :: iodata()
  def request(kind, source, language, opts \\ [])
      when kind in [:document, :lines] and is_binary(source) and is_binary(language) do
    [
      <<kind_byte(kind), Keyword.get(opts, :match_limit, 0)::32,
        Keyword.get(opts, :time_limit, 0)::32, Keyword.get(opts, :cpu_limit_ms, 0)::32,
        byte_size(language)::16>>,
      language,
      source
    ]
  end

  defp kind_byte(:document), do: @document
  defp kind_byte(:lines), do: @lines

  @doc """
  Decodes a reply `lumis serve` sent to a request of `kind`.
  """
  @spec decode_reply(binary(), kind()) :: reply()
  def decode_reply(<<@ok, max_rss_kb::32, html::binary>>, :document),
    do: {:ok, html, max_rss_kb}

  def decode_reply(<<@ok, max_rss_kb::32, body::binary>>, :lines) do
    case decode_lines(body, []) do
      {:ok, lines} -> {:ok, lines, max_rss_kb}
      :error -> {:error, :malformed_reply}
    end
  end

  def decode_reply(<<@error, max_rss_kb::32, message::binary>>, _kind),
    do: {:error, message, max_rss_kb}

  def decode_reply(<<@ready, max_rss_kb::32>>, _kind), do: {:ready, max_rss_kb}
  def decode_reply(_reply, _kind), do: {:error, :malformed_reply}

  defp decode_lines(<<>>, lines), do: {:ok, Enum.reverse(lines)}

  defp decode_lines(<<size::32, line::binary-size(size), rest::binary>>, lines),
    do: decode_lines(rest, [line | lines])

  defp decode_lines(_body, _lines), do: :error
end
