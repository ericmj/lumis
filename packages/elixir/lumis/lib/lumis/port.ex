defmodule Lumis.Port do
  @moduledoc """
  Highlights in a separate `lumis serve` OS process, over an Erlang port.

  The NIF highlights inside the VM, where a highlight cannot be stopped once it
  has started and a crash in the parser takes the VM with it. `lumis serve` runs
  the same highlighting in its own process: closing the port stops it, also in
  the middle of a highlight, and a crash only ends that process.

      port = Lumis.Port.open(preload: ["elixir"])
      Port.command(port, Lumis.Port.request("IO.puts(:ok)", "lib/app.ex"))

      receive do
        {^port, {:data, reply}} -> Lumis.Port.decode_reply(reply)
      end
      #=> {:ok, "<pre class=\\"lumis\\">...", [], 31_412}

  Output is html-linked HTML, the same as
  `Lumis.highlight!(source, formatter: {:html_linked, language: language})`.

  The process never downloads. Languages it does not find in the data
  directory come back as `:not_cached`, or in the list of missing injections,
  and `cache/2` fetches them.
  """

  @highlight 1

  @typedoc "Peak resident set size of the `lumis serve` process, in kilobytes."
  @type max_rss_kb() :: non_neg_integer()

  @type reply() ::
          {:ok, html :: String.t(), missing :: [String.t()], max_rss_kb()}
          | {:not_cached, language :: String.t(), max_rss_kb()}
          | {:error, message :: String.t(), max_rss_kb()}

  @type env() :: [{String.t(), String.t() | false}]

  @doc """
  Path to the `lumis` executable this package builds.
  """
  @spec executable() :: Path.t()
  def executable do
    Application.app_dir(:lumis, "priv/native/lumis")
  end

  @doc """
  The data directory `lumis` reads parsers from: `config :lumis, :data_dir`,
  then `LUMIS_DATA_DIR`, then the application's `priv/lumis`, the same order
  `Lumis.Application` configures the NIF with.
  """
  @spec data_dir() :: Path.t()
  def data_dir do
    cond do
      path = Application.get_env(:lumis, :data_dir) -> Path.expand(path)
      (path = System.get_env("LUMIS_DATA_DIR")) not in [nil, ""] -> path
      true -> Application.app_dir(:lumis, "priv/lumis")
    end
  end

  @doc """
  Starts `lumis serve` and returns its port, owned by the calling process.

  The port sends `{port, {:data, reply}}` for every request, and
  `{port, {:exit_status, status}}` when the process exits.

  ## Options

    * `:preload` - languages to load before the first request.
    * `:data_dir` - defaults to `data_dir/0`.
    * `:wrapper` - a command and arguments to run `lumis` under, such as
      `["nice", "-n", "10", "--"]`. It has to exec `lumis` rather than fork it,
      so the port's OS process is `lumis` itself and closing the port reaches it.
    * `:env` - environment variables to set, or to unset with `false`. Without
      it, `lumis` inherits the whole environment of the VM.

  """
  @spec open(keyword()) :: port()
  def open(opts \\ []) do
    command =
      Keyword.get(opts, :wrapper, []) ++
        [executable(), "--data-dir", Keyword.get_lazy(opts, :data_dir, &data_dir/0), "serve"] ++
        preload_args(Keyword.get(opts, :preload, []))

    [program | args] = command

    Port.open({:spawn_executable, program}, [
      :binary,
      :exit_status,
      :use_stdio,
      :hide,
      packet: 4,
      args: args,
      env: Enum.map(Keyword.get(opts, :env, []), &port_env/1)
    ])
  end

  defp preload_args([]), do: []
  defp preload_args(languages), do: ["--preload", Enum.join(languages, ",")]

  defp port_env({name, false}), do: {String.to_charlist(name), false}
  defp port_env({name, value}), do: {String.to_charlist(name), String.to_charlist(value)}

  @doc """
  Encodes a request to highlight `source` as `language`, a language name or a
  file path.

  ## Options

    * `:match_limit` - the query match limit, as `Lumis.highlight/2` takes it.
      Defaults to the Tree-sitter default.
    * `:cpu_limit_ms` - CPU time the request may use. Past it, the kernel kills
      the process with `SIGXCPU`. It is enforced in whole seconds, so it rounds
      up. Defaults to no limit.

  """
  @spec request(String.t(), String.t(), keyword()) :: iodata()
  def request(source, language, opts \\ []) when is_binary(source) and is_binary(language) do
    match_limit = Keyword.get(opts, :match_limit, 0)
    cpu_limit_ms = Keyword.get(opts, :cpu_limit_ms, 0)

    [
      <<@highlight, match_limit::32, cpu_limit_ms::32, byte_size(language)::16>>,
      language,
      source
    ]
  end

  @doc """
  Decodes a reply `lumis serve` sent.

  `missing` lists languages injected in the source that are not cached; their
  blocks are unhighlighted in `html`.
  """
  @spec decode_reply(binary()) :: reply()
  def decode_reply(<<status, max_rss_kb::32, count::16, rest::binary>>) do
    {missing, body} = decode_names(count, rest, [])

    case status do
      0 -> {:ok, body, missing, max_rss_kb}
      1 -> {:error, body, max_rss_kb}
      2 -> {:not_cached, hd(missing), max_rss_kb}
    end
  end

  defp decode_names(0, rest, names), do: {Enum.reverse(names), rest}

  defp decode_names(count, <<size::16, name::binary-size(size), rest::binary>>, names) do
    decode_names(count - 1, rest, [name | names])
  end

  @doc """
  Downloads and compiles `languages` into the data directory with
  `lumis languages cache`, so `lumis serve` processes find them.

  ## Options

    * `:data_dir` - defaults to `data_dir/0`.
    * `:env` - as for `open/1`.

  """
  @spec cache([String.t()], keyword()) :: :ok | {:error, String.t()}
  def cache(languages, opts \\ []) when is_list(languages) do
    args =
      ["--data-dir", Keyword.get_lazy(opts, :data_dir, &data_dir/0), "languages", "cache"] ++
        languages

    env = Enum.map(Keyword.get(opts, :env, []), fn {name, value} -> {name, value || nil} end)

    case System.cmd(executable(), args, env: env, stderr_to_stdout: true) do
      {_output, 0} -> :ok
      {output, _status} -> {:error, output}
    end
  end
end
